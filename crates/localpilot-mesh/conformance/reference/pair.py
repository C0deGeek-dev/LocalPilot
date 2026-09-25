#!/usr/bin/env python3
from __future__ import annotations
import argparse, hashlib, hmac, json, os, re, secrets, subprocess, sys, tempfile, time, uuid
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

# Mailbox content is UTF-8 on disk, but Python encodes stdout with the locale
# codepage when it is a pipe rather than a console — cp1252 on a default Windows
# install. A peer message containing an arrow or an em-dash then killed `watch`
# with UnicodeEncodeError, and the exit code was 1: indistinguishable from a
# benign watch timeout. Relay the bytes the mailbox actually holds.
for _s in (sys.stdout, sys.stderr):
    try: _s.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError, OSError): pass

ROLES=("claude","codex")
KINDS={"HELLO","PLAN","CHALLENGE","DESIGN_AGREED","CHECKPOINT","STOP","STEER","NOTE","QUESTION","ANSWER","REVIEW_REQUEST","VERDICT","HANDOFF_OFFER","HANDOFF_ACCEPT","ESCALATE","COMPLETE"}
HEALTH={"not_joined","ready","working","waiting","rate_limited","paused","offline"}
PAIR_DIR=".pair-programming"
COMPANION_FILE=".pair-companion.json"
PAIRIGNORE=".pairignore"
# Which backend a mailbox was started on, recorded once inside the mailbox. A
# mailbox predating this file has no marker and reads as "git", which is what it
# was, so existing sessions keep working unchanged.
VCS_FILE="vcs.json"
TERMINAL={"completed","abandoned"}
EXCLUDE_MARK="# added by pair-programming; removed by `pair.py purge`"
NO_EOL_NOTE=" (this file had no trailing newline; purge restores that)"
# Exactly the two lines this tool writes. A prefix test would have claimed any
# user comment that merely began the same way — including an annotated copy of
# our own marker — and then deleted their rule as ours.
TOOL_MARKS=(EXCLUDE_MARK,EXCLUDE_MARK+NO_EOL_NOTE)
# What this build can honour, advertised by `start` and `join`. Acknowledged
# delivery is negotiated, never assumed: a peer on an older build ignores the
# session field and would keep advancing its cursor on print, so a session only
# switches to `ack` once both roles have advertised it.
CAPS=["ack"]
# The mailbox protocol this build speaks (references/spec.md), and the optional
# features it implements. A record may name features it `requires`; an unknown
# one, or another major version, is refused rather than guessed at.
PROTOCOL="1.0"
FEATURES=frozenset()
# Identities a session may be made of, and the historic pair. A session of
# exactly the historic pair keeps schema 1 for its whole life: the legacy file
# layout (JSON `active.json`, `session.json`) and never the schema-2 keys
# (`schema`, `participants`). Newer builds may add keys older builds ignore.
# Anything else is schema 2, which an older build must not be able to open
# (see `MB.point`).
PARTICIPANTS=("claude","codex","localpilot")
HISTORIC=("claude","codex")
SESSION_V1="session.json"
SESSION_V2="session.v2.json"
ACTIVE_V1="active.json"
ACTIVE_V2="active.v2.json"
# Deliberately not JSON. An older build parses active.json on every command
# that could create, join or change a session, so a schema-2 session makes it
# stop with a decode error instead of reading two of three participants.
SENTINEL_PREFIX="N-PARTY SESSION "

def schema(s): return int(s.get("schema") or 1)

def participants(s):
    """The session's identities, in their fixed order. Schema 1 derives them."""
    return list(s.get("participants") or (s["driver"],s["navigator"]))

def session_file(d,want=None):
    """The one session file in a session directory.

    No valid directory holds both files. One that does is ambiguous, and which
    record a reader sees must never depend on which name it happened to try
    first, so it is refused. `want` (1 or 2) is the schema the caller's pointer
    names; the file must match it."""
    v1,v2=(d/SESSION_V1).exists(),(d/SESSION_V2).exists()
    if v1 and v2: raise SystemExit(f"ambiguous session directory {d.name!r}: holds both {SESSION_V1} and {SESSION_V2}")
    if want==1: return d/SESSION_V1
    if want==2: return d/SESSION_V2
    return d/SESSION_V2 if v2 else d/SESSION_V1

def now(): return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00","Z")
# The only kind `post --broadcast` accepts. Authority notices (handoff, unit
# close) are broadcast only by their own validated commands, never by a post,
# so no participant can forge one; the join notice is internal.
BROADCAST_KINDS={"ESCALATE"}

# Characters that str.splitlines() treats as line breaks but json.dumps(...,
# ensure_ascii=False) leaves raw inside strings. Unescaped, a body holding one
# splits its journal record in two, both halves fail to parse, and the message
# vanishes without an error.
RAW_BREAKS={"\u2028":"\\u2028","\u2029":"\\u2029","\x85":"\\u0085"}

def journal_record(m):
    """One journal line: compact JSON with every raw line-break escaped."""
    line=json.dumps(m,ensure_ascii=False,separators=(",",":"))
    for raw,esc in RAW_BREAKS.items(): line=line.replace(raw,esc)
    return line+"\n"

def journal_lines(p):
    """A journal's lines, split on "\n" only, so a record written by an older
    build with a raw U+2028, U+2029 or U+0085 in it still reads as one record.

    The bytes are split first and each line is decoded on its own: a crash can
    cut a multi-byte character in half, and decoding the whole file then failed
    for every reader, including the writer about to seal that very tail. A line
    that is not valid UTF-8 comes back as None, an invalid line."""
    out=[]
    for raw in p.read_bytes().split(b"\n"):
        try: out.append(raw.decode("utf-8"))
        except UnicodeDecodeError: out.append(None)
    return out

def check_protocol(rec,where):
    """`rec` unchanged, or a refusal if it needs a protocol this build lacks.

    A missing version is 1.0, the protocol every earlier build spoke. Another
    major version, or a required feature this build does not know, fails closed:
    operating a session by rules it does not understand is how state gets
    corrupted quietly. An unknown minor version is additive by definition, so it
    is accepted, and so is any optional key."""
    if not isinstance(rec,dict): return rec
    v=rec.get("protocol")
    if v is not None:
        major=str(v).split(".",1)[0]
        if major!=PROTOCOL.split(".",1)[0]:
            raise SystemExit(f"{where} uses protocol {v}; this build speaks {PROTOCOL.split('.',1)[0]}.x. "
                             "Use a build that supports it; nothing was changed")
    unknown=[f for f in (rec.get("requires") or []) if f not in FEATURES]
    if unknown:
        raise SystemExit(f"{where} requires {', '.join(map(str,unknown))}, which this build does not support; nothing was changed")
    return rec

def _record(line):
    """The JSON object on a journal line, or None for an invalid line. A record
    that needs a protocol this build lacks is refused, not skipped."""
    if line is None: return None
    try: m=json.loads(line)
    except json.JSONDecodeError: return None
    return check_protocol(m,"a journal record") if isinstance(m,dict) else None

def journal_records(p):
    """Every valid record in a journal, in file order. A line that is blank,
    is not UTF-8, is not JSON, or is JSON but not an object is skipped: `null`
    or a bare number used to reach `.get()` and crash every reader."""
    return [m for m in map(_record,journal_lines(p)) if m is not None]

def journal_invalid(p):
    """The line numbers of the invalid lines, so damage is reported rather than
    skipped in silence. The empty string after a final newline is not a line."""
    lines=journal_lines(p)
    if lines and lines[-1]=="": lines=lines[:-1]
    return [n for n,line in enumerate(lines,1) if _record(line) is None]

def seal_torn_tail(p):
    """Before an append: if the journal does not end in a newline, end it.

    A crash mid-append leaves a partial last line. Appending straight after it
    fused the next record onto the fragment, and both were then skipped as one
    unparseable line, so the new message vanished. Sealing turns the fragment
    into its own invalid line, which readers skip and `status` reports. Called
    under the role lock, like the append itself."""
    try:
        with open(p,"rb") as f:
            f.seek(0,os.SEEK_END)
            if f.tell()==0: return
            f.seek(-1,os.SEEK_END)
            if f.read(1)==b"\n": return
    except FileNotFoundError: return
    with open(p,"ab") as f: f.write(b"\n"); f.flush(); os.fsync(f.fileno())

def find_msg(mb,s,msg_id):
    """The message `<role>:<seq>` in this session, or None."""
    m=re.fullmatch(r"([a-z]+):([1-9][0-9]*)",msg_id or "")
    if not m or m.group(1) not in participants(s): return None
    p=mb.journal(s,m.group(1))
    if not p.exists(): return None
    for x in journal_records(p):
        if int(x.get("seq",0))==int(m.group(2)): return x
    return None

def visible(m,role):
    """Mail a role may see and refer to: its own, addressed to it, or broadcast."""
    return m.get("role")==role or role in (m.get("to") or []) or bool(m.get("broadcast"))

def _check_recipients(s,role,named):
    """Every resolved recipient list, explicit or inferred, obeys the same rules."""
    for x in named:
        if x not in participants(s): raise SystemExit(f"unknown recipient {x!r} (participants: {', '.join(participants(s))})")
        if x==role: raise SystemExit("a participant cannot address itself")
    if len(set(named))!=len(named): raise SystemExit("--to names a recipient twice")

FORWARD_TTL=2   # forward edges a request may take from its author

def waiting_map(s):
    """Schema 2's waits: {msg_id: {from, kind, since, pending[], answered_by[]}}.

    A schema-2 pair written before the map held one scalar record; it is read
    as the equivalent single entry rather than dropped."""
    w=s.get("waiting")
    if not w: return {}
    if "from_role" in w:
        mid=f"{w['from_role']}:{w['seq']}"
        return {mid:{"from":w["from_role"],"kind":w.get("kind"),"since":w.get("since"),
                     "pending":[w.get("for_role")],"answered_by":[]}}
    return w

def unread_for(mb,s,role):
    """[(sender, first, last)] of mail addressed to `role` it has not consumed.

    "Consumed" is the reader's own per-sender cursor (acknowledged under ack
    delivery), so mail an unattended reader printed still counts. Read-only:
    it is what a wake adapter asks before waking the agent."""
    if schema(s)==1:
        peer=the_peer(s,role); seen=int((readj(mb.cursor(s,role),{}) or {}).get("peer_seq",0))
        seqs=[int(m["seq"]) for m in journal_after(mb,s,peer,seen)]
        return [(peer,min(seqs),max(seqs))] if seqs else []
    c=v2_cursor(s,role,readj(mb.cursor(s,role),{}) or {}); out=[]
    for snd in [r for r in participants(s) if r!=role]:
        seqs=[int(m["seq"]) for m in journal_after(mb,s,snd,int(c["from"][snd].get("peer_seq",0))) if visible(m,role)]
        if seqs: out.append((snd,min(seqs),max(seqs)))
    return out

def address(mb,s,role,kind,expect,to,reply_to,broadcast,internal=False,forward=False):
    """Recipients and lineage for a post, validated on snapshot `s`.

    Schema 1 is the historic pair and keeps its implicit peer; directed-mail
    flags are refused there rather than silently ignored. Schema 2 resolves
    `to[]` from the flags, and returns the fields the message will carry."""
    others=[r for r in participants(s) if r!=role]
    if schema(s)==1:
        if to or reply_to or broadcast or forward:
            raise SystemExit("--to, --reply-to and --broadcast need an N-party session (start it with --with)")
        the_peer(s,role)
        return {}
    if broadcast and forward:
        raise SystemExit("--forward and --broadcast are exclusive: a forward goes to named recipients")
    if broadcast:
        # A broadcast is a new notice, never part of a thread: a threaded
        # escalation is directed to the thread's origin instead.
        if reply_to: raise SystemExit("--broadcast and --reply-to are exclusive; a threaded ESCALATE goes to the thread origin")
        if not internal and kind not in BROADCAST_KINDS:
            raise SystemExit(f"--broadcast is allowed only for {', '.join(sorted(BROADCAST_KINDS))}; "
                             f"{kind} notices come from their own commands")
        if expect: raise SystemExit("--broadcast cannot --expect-reply: a notice must not create a wait")
        if to: raise SystemExit("--broadcast and --to are exclusive")
        return {"to":others,"broadcast":True,"reply_to":reply_to,"route_trace":[role],"ttl":None,"thread_id":None}
    named=[x.strip() for x in (to or "").split(",") if x.strip()]
    if forward:
        # A forward carries a request on to someone else. It must name who, and
        # it must reference mail the forwarder actually received. Its route may
        # not revisit anyone already on it, and it spends one edge of the ttl.
        if not reply_to: raise SystemExit("--forward needs --reply-to <msg_id> (the message being forwarded)")
        if not named: raise SystemExit("--forward needs --to (who the request goes to next)")
        mref=find_msg(mb,s,reply_to)
        if not mref or not visible(mref,role):
            raise SystemExit(f"--reply-to {reply_to}: no such message visible to {role} in this session")
        _check_recipients(s,role,named)
        route=list(mref.get("route_trace") or [mref["role"]])
        # The forwarder must be receiving this request for the first time. An
        # author re-sending its own request, or anyone passing a reply back down
        # the route it already travelled, would duplicate itself on the route.
        if role in route:
            raise SystemExit(f"{role} is already on this request's route ({' -> '.join(route)}); "
                             f"forward only a request you received. Reply instead, or ESCALATE to the thread origin")
        trace=route+[role]
        ttl=mref.get("ttl"); ttl=FORWARD_TTL if ttl is None else int(ttl)
        if ttl<=0: raise SystemExit(f"{reply_to} may not be forwarded again (ttl exhausted); reply to it, or ESCALATE to the thread origin")
        loop=[x for x in named if x in trace]
        if loop: raise SystemExit(f"{', '.join(loop)} already on this request's route ({' -> '.join(trace)}); "
                                  f"a forward cannot loop. Reply instead, or ESCALATE to the thread origin")
        return {"to":named,"broadcast":False,"reply_to":reply_to,"thread_id":mref.get("thread_id") or reply_to,
                "route_trace":trace,"ttl":ttl-1,"forward":True}
    for x in named:
        if x not in participants(s): raise SystemExit(f"unknown recipient {x!r} (participants: {', '.join(participants(s))})")
        if x==role: raise SystemExit("a participant cannot address itself")
    if len(set(named))!=len(named): raise SystemExit("--to names a recipient twice")
    if reply_to:
        mref=find_msg(mb,s,reply_to)
        if not mref or not visible(mref,role):
            raise SystemExit(f"--reply-to {reply_to}: no such message visible to {role} in this session")
        author=mref["role"]
        if kind=="ESCALATE" and not named:
            origin=(mref.get("thread_id") or reply_to).split(":")[0]
            named=[origin] if origin!=role else [author]
        if not named: named=[author]
        if kind!="ESCALATE" and named!=[author]:
            raise SystemExit(f"a reply goes to the author of {reply_to} ({author}); reaching anyone else is a forward")
        if author==role and kind!="ESCALATE": raise SystemExit("a reply to your own message has no recipient")
        _check_recipients(s,role,named)          # again, now that defaults are resolved
        return {"to":named,"broadcast":False,"reply_to":reply_to,"thread_id":mref.get("thread_id") or reply_to,
                "route_trace":list(mref.get("route_trace") or [author]),"ttl":mref.get("ttl")}
    if not named:
        if len(others)!=1: raise SystemExit(f"--to is required with {len(participants(s))} participants")
        named=others
    _check_recipients(s,role,named)
    return {"to":named,"broadcast":False,"reply_to":None,"thread_id":None,"route_trace":[role],"ttl":None}

def the_peer(s,role):
    """The one other participant of a two-participant session.

    Every path that still assumes exactly one peer goes through here, so a
    three-party session reaching it stops loudly instead of silently reading
    one of two peers. A two-participant session of any identities (including
    localpilot) resolves correctly."""
    ps=[r for r in participants(s) if r!=role]
    if len(ps)!=1:
        raise SystemExit(f"not supported for three-party sessions yet: this command assumes one peer "
                         f"(participants: {', '.join(participants(s))})")
    return ps[0]

def authority(s):
    """Who owns the current unit, who must agree before it closes, who advises.

    `owner` stays the one writable owner field; this only derives the rest.
    Schema 1 stores nothing: its one peer is the required reviewer, exactly
    the historic rule. Schema 2 freezes the declaration per unit."""
    o=s["owner"]; rec=s.get("authority") or {}
    adv=list(rec.get("advisers") or [])
    req=rec.get("required_reviewers")
    if req is None: req=[r for r in participants(s) if r!=o and r not in adv]
    return {"owner":o,"required_reviewers":list(req),"advisers":adv}

def declare_authority(parts,owner,advisers):
    """Validate an --advisers declaration and return the record to freeze.

    Every non-owner participant not named as an adviser is a required
    reviewer. A declaration that leaves nobody required is refused: a unit
    nobody must agree to could be closed on the owner's word alone."""
    named=[x.strip() for x in (advisers or "").split(",") if x.strip() and x.strip()!="none"]
    if len(set(named))!=len(named): raise SystemExit("--advisers names a participant twice")
    for x in named:
        if x not in parts: raise SystemExit(f"adviser {x} is not a participant (participants: {', '.join(parts)})")
        if x==owner: raise SystemExit(f"adviser {x} is the owner; the owner cannot advise on its own unit")
    req=[x for x in parts if x!=owner and x not in named]
    if not req: raise SystemExit("--advisers leaves no required reviewer; at least one non-owner participant must be required to agree")
    return {"required_reviewers":req,"advisers":[x for x in parts if x in named]}

def handoff_authority(s,new_owner):
    """The reviewer sets after `new_owner` takes the unit over, or a refusal.

    - The incoming owner was a required reviewer: it leaves the set. The old
      owner wrote the work, so it is never the independent reviewer; it
      becomes a required reviewer only when nobody else is left, and an
      adviser otherwise.
    - The incoming owner was an adviser: the required reviewers are unchanged
      and the old owner takes the adviser slot.

    Nobody who is a required reviewer stops being one, except by becoming the
    owner, and the result is never an empty required set."""
    a=authority(s); old=s["owner"]; order=participants(s)
    req=[r for r in a["required_reviewers"] if r!=new_owner]; adv=[r for r in a["advisers"] if r!=new_owner]
    if new_owner in a["required_reviewers"] and not req: req.append(old)
    else: adv.append(old)
    if not req: raise SystemExit("this handoff would leave no required reviewer; refusing")
    return {"required_reviewers":[r for r in order if r in req],"advisers":[r for r in order if r in adv]}

def pauses(s):
    """Every recorded quota pause, by participant.

    Schema 1 keeps its single `pause` record, so an older build reads the same
    file it always did. Schema 2 keeps one record per participant, so two
    participants can be rate-limited at once. A schema-2 session written before
    the per-participant map existed still holds a single `pause`; it is read
    as a one-entry map, so a blocking pause from that build never vanishes."""
    if schema(s)==2 and "pauses" in s: return dict(s.get("pauses") or {})
    p=s.get("pause"); return {p["role"]:p} if p else {}

def _migrate_pauses(s):
    """Move a schema-2 session's legacy single `pause` into the map, once."""
    if schema(s)==2 and "pauses" not in s:
        s["pauses"]=pauses(s); s.pop("pause",None)

def pause_blocks(s,role):
    """Whether `role`'s pause stops the unit: the owner's or a required reviewer's
    does; an adviser's is informational."""
    a=authority(s); return role==a["owner"] or role in a["required_reviewers"]

def settle_status(s):
    """Derive active/paused from the pauses and the unit's authority.

    The one place the session's paused state is decided. Every write that
    changes a pause, the owner or the reviewer sets calls it, so the status
    can never disagree with who may block: guard-write reads it."""
    if s.get("status") not in ("active","paused"): return s
    s["status"]="paused" if any(pause_blocks(s,r) for r in pauses(s)) else "active"
    return s

def set_pause(s,role,rec):
    _migrate_pauses(s)
    if schema(s)==2: s["pauses"]={**(s.get("pauses") or {}),role:rec}
    else: s["pause"]=rec
    return settle_status(s)

def clear_pause(s,role):
    _migrate_pauses(s)
    if schema(s)==2:
        ps=dict(s.get("pauses") or {}); ps.pop(role,None); s["pauses"]=ps
    elif (s.get("pause") or {}).get("role")==role: s["pause"]=None
    return settle_status(s)

def pause_line(s,role,rec):
    return (f"PAUSE {role} {'blocking' if pause_blocks(s,role) else 'informational'} "
            f"reason={rec.get('reason') or '-'} resume_at={rec.get('resume_at') or '-'}")

def authority_line(owner,rec,label=None):
    return (f"AUTHORITY{' unit='+label if label else ''} owner={owner} required={','.join(rec.get('required_reviewers') or []) or '-'} "
            f"advisers={','.join(rec.get('advisers') or []) or '-'}")
def parse_ts(v):
    if not v: return None
    if v.isdigit(): return float(v)
    return datetime.fromisoformat(v.replace("Z","+00:00")).timestamp()
def fmt_ts(v): return datetime.fromtimestamp(v,timezone.utc).isoformat(timespec="seconds").replace("+00:00","Z") if v else None

def git(repo,*args,check=True):
    # GIT_OPTIONAL_LOCKS=0 stops `git status` refreshing (and rewriting) the index
    # as a side effect. A command that advertises itself as read-only must not
    # touch .git, and `verify-request` makes exactly that claim.
    env={**os.environ,"GIT_OPTIONAL_LOCKS":"0"}
    # Decode as UTF-8, not the locale codepage. Git emits path bytes as stored,
    # and `text=True` alone decodes them with cp1252 on a default Windows install
    # — so a filename with an accent came back mojibake and then "did not exist".
    try:
        p=subprocess.run(["git","-C",str(repo),*args],text=True,encoding="utf-8",errors="replace",
                         stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
    except OSError:
        # No git executable at all. Every `check=False` caller is asking "is this
        # a working tree", and for those the honest answer is no. Raising instead
        # would make `--no-vcs` — whose entire purpose is not needing Git — fail
        # on a machine without Git installed.
        if check: raise SystemExit("git is not available on PATH")
        return ""
    if check and p.returncode: raise SystemExit(p.stderr.strip() or "git failed")
    return p.stdout.rstrip("\n")

def find_mailbox(start):
    """The nearest ancestor holding a mailbox, or None.

    Git mode never needs this — `rev-parse --show-toplevel` answers from any
    subdirectory — but a no-VCS tree has no such oracle, and the two terminals
    still have to agree on one root no matter which directory each was opened
    in. The mailbox is that anchor: `start` creates it once, at the root."""
    p=Path(start).resolve()
    for q in (p,*p.parents):
        if (q/PAIR_DIR).is_dir(): return q
    return None

def mailbox_vcs(repo):
    """The backend this mailbox was started on: "git" or "none".

    Frozen, for the same reason companions are frozen. A `git init` run later in
    a directory that was deliberately paired without version control must not
    move the review boundary under a session already running on it."""
    m=readj(Path(repo)/PAIR_DIR/VCS_FILE)
    return "none" if isinstance(m,dict) and m.get("vcs")=="none" else "git"

def detect_vcs(p):
    """Live detection, for a path with no mailbox of its own — a companion."""
    return "git" if git(p,"rev-parse","--show-toplevel",check=False) else "none"

def live_session_at(mb_root):
    """True when this mailbox still holds a session that is not finished with.

    Parked and paused both count: a parked session is resumable work, and a
    paused one is live work whose peer is down. Only `completed` and `abandoned`
    are finished — the same line `purge` draws."""
    # A mailbox whose pointer cannot be resolved is treated as live: the
    # conservative answer, because "live" keeps its anchor rather than letting a
    # later `git init` capture it.
    try: s=MB(Path(mb_root)).active()
    except SystemExit: return True
    return isinstance(s,dict) and s.get("status") not in TERMINAL

def root(path=None,no_vcs=False):
    p=Path(path or os.getcwd()).resolve()
    mb=find_mailbox(p)
    # A no-VCS mailbox outranks live detection *while a session is running in
    # it*, so a `git init` landing mid-session cannot re-anchor a pair whose
    # every pin was taken against the directory. Once that session is completed
    # or abandoned the mailbox is an archive, and an archive must not capture a
    # repository created or cloned underneath it later: that path ran a real
    # repository's session in no-VCS mode, anchored to an ancestor directory
    # nobody named, without saying so. The archive stays reachable through the
    # branches below, where there is no Git to prefer, so `status`, `transcript`
    # and `purge` still find it.
    if mb and mailbox_vcs(mb)=="none" and live_session_at(mb): return mb
    # git's own "fatal: not a git repository" names neither the tool that asked
    # nor why it cares, which is unhelpful when the asker is a skill the user
    # just invoked and the answer is "point it at a working tree".
    top=git(p,"rev-parse","--show-toplevel",check=False)
    if no_vcs:
        if top: raise SystemExit(f"--no-vcs was passed inside the Git working tree at {top}; version control that exists must not be ignored, because the review boundary would silently stop using it. Drop the flag, or run outside the tree")
        # Only a Git mailbox is a conflict. A no-VCS one whose last session is
        # finished is exactly where a new `--no-vcs` session belongs.
        if mb and mailbox_vcs(mb)=="git": raise SystemExit(f"the mailbox at {mb} was started under Git; --no-vcs cannot change the backend of a mailbox that already has one")
        if mb: return mb
        return p
    if top: return Path(top).resolve()
    if mb: return mb
    raise SystemExit(f"pair-programming found no Git repository and no existing mailbox at {p}. Either point it at a working tree, or start the session with `--no-vcs` to anchor ownership to a content digest of this directory instead of Git HEAD")
def atomic(path,obj):
    path.parent.mkdir(parents=True,exist_ok=True)
    data=json.dumps(obj,ensure_ascii=False,separators=(",",":"))+"\n"
    fd,tmp=tempfile.mkstemp(prefix=path.name+".",suffix=".tmp",dir=path.parent)
    try:
        with os.fdopen(fd,"w",encoding="utf-8",newline="\n") as f:
            f.write(data); f.flush(); os.fsync(f.fileno())
        # Windows refuses a replace while another process has the target open
        # (a reader, or a concurrent replace): a transient sharing violation, not
        # a real permission failure. Retry briefly, then fail as before.
        for i in range(40):
            try: os.replace(tmp,path); break
            except PermissionError:
                if i==39: raise
                time.sleep(.05)
    finally:
        try: os.unlink(tmp)
        except FileNotFoundError: pass
def atomic_text(path,text):
    """`atomic` for a plain-text file (the schema-2 sentinel)."""
    path.parent.mkdir(parents=True,exist_ok=True)
    fd,tmp=tempfile.mkstemp(prefix=path.name+".",suffix=".tmp",dir=path.parent)
    try:
        with os.fdopen(fd,"w",encoding="utf-8",newline="\n") as f:
            f.write(text); f.flush(); os.fsync(f.fileno())
        for i in range(40):
            try: os.replace(tmp,path); break
            except PermissionError:
                if i==39: raise
                time.sleep(.05)
    finally:
        try: os.unlink(tmp)
        except FileNotFoundError: pass

def read_text_retry(path):
    """A file's text, or None if absent, riding out a transient Windows sharing
    violation exactly as `readj` does. The pointer is read raw (the schema-2
    sentinel is not JSON), and a raw read must not be the one place where a
    concurrent `atomic` replace fails the command."""
    for i in range(40):
        try: return path.read_text(encoding="utf-8")
        except FileNotFoundError: return None
        except PermissionError:
            if i==39: raise
            time.sleep(.05)

def mailbox_rel(path):
    """`path` relative to the mailbox directory when it is inside one, for
    messages that must not depend on where the repository lives."""
    parts=Path(path).parts
    return "/".join(parts[parts.index(PAIR_DIR)+1:]) if PAIR_DIR in parts else str(path)

def readj(path,default=None):
    # Deliberately narrow. Swallowing decode/OS errors here reads a corrupt
    # active session as "no active session", and `start` then overwrites the
    # pointer and orphans live work — a data-loss path dressed as robustness.
    # Tolerance for unreadable state belongs in `scan_sessions`, which decides
    # per caller whether skipping or refusing is correct.
    # The one exception retried: on Windows a read that lands while `atomic`
    # replaces the file fails with a sharing violation. That is transient, and
    # it is still raised if it persists.
    for i in range(40):
        try: return json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError: return default
        except json.JSONDecodeError as e:
            # Still a JSONDecodeError, so callers that decide per case keep
            # working, but one that names the record: an uncaught one used to
            # surface as a bare traceback.
            raise json.JSONDecodeError(f"{mailbox_rel(path)} is not valid JSON ({e.msg})",e.doc,e.pos) from None
        except PermissionError:
            if i==39: raise
            time.sleep(.05)

def exclude_path(repo):
    """The lexical path to info/exclude — deliberately not `.resolve()`d.

    Resolving erases symlink identity: if the file is a link, the resolved path
    names its target and every later `reparse` check answers about the target
    instead. Joining without resolving keeps the link visible so it can be
    refused."""
    p=Path(git(repo,"rev-parse","--git-path","info/exclude"))
    return p if p.is_absolute() else (repo/p)

def exclude(repo,mode=None):
    """Ignore the mailbox, and record that we were the one who said so.

    The marker is a plain comment line, which Git honours and ignores, sitting
    directly above the rule. It goes here rather than inside the mailbox for two
    reasons: a record in `.pair-programming/` would count toward "is the container
    empty" when the purge asks, and it would vanish exactly when the mailbox is
    removed - which is the moment the answer is needed.

    In no-VCS mode there is nothing tracking this directory, so there is nothing
    to tell to ignore the mailbox, and no info/exclude to write to. The mode is
    passed in rather than read back from the mailbox: `start` calls this before
    the marker exists, and a default of "git" would then write an ignore rule
    into a repository the user is not in."""
    if (mode or mailbox_vcs(repo))=="none": return
    p=exclude_path(repo)
    p.parent.mkdir(parents=True,exist_ok=True)
    cur=p.read_text(encoding="utf-8",errors="replace") if p.exists() else ""
    rule="/"+PAIR_DIR+"/"
    if rule not in cur.splitlines():
        # If the file had no final newline we must add one to start our block,
        # and removing the block then has to put the file back exactly as it
        # was. A note on the marker records that we supplied that separator;
        # without it, purge left behind a trailing newline the user never wrote
        # and the byte-preservation promise was quietly false.
        needs_sep=bool(cur) and not cur.endswith(("\n","\r"))
        with p.open("a",encoding="utf-8",newline="\n") as f:
            if needs_sep: f.write("\n")
            f.write((EXCLUDE_MARK+NO_EOL_NOTE if needs_sep else EXCLUDE_MARK)+"\n"+rule+"\n")

def glob_re(pat):
    """A glob over `/`-separated path segments, as a regex.

    `fnmatch` is not usable here: its `*` matches `/` too, so `plans/*` would
    authorize `plans/a/b/c` and the allowlist would be wider than it reads.
    `**` spans segments; `*` and `?` never cross one."""
    out=["(?s:"]; i=0; n=len(pat)
    while i<n:
        c=pat[i]
        if c=="*":
            if pat[i:i+3]=="**/": out.append("(?:[^/]+/)*"); i+=3; continue
            if pat[i:i+2]=="**": out.append(".*"); i+=2; continue
            out.append("[^/]*"); i+=1; continue
        if c=="?": out.append("[^/]"); i+=1; continue
        if c=="[":
            j=i+1
            if j<n and pat[j] in "!^": j+=1
            if j<n and pat[j]=="]": j+=1
            while j<n and pat[j]!="]": j+=1
            if j>=n: out.append(re.escape(c)); i+=1; continue
            inner=pat[i+1:j]
            out.append("["+("^"+inner[1:] if inner[:1] in ("!","^") else inner)+"]"); i=j+1; continue
        out.append(re.escape(c)); i+=1
    out.append(r")\Z")
    return re.compile("".join(out))

def glob_match(pats,rel):
    """True when `rel` — POSIX form, relative to the companion root — is allowed.

    Matching is case-sensitive on every platform. A case-insensitive filesystem
    can therefore refuse a write whose glob differs only in case: the safe
    direction, and a deterministic one. A protocol rule that meant different
    things on Windows and Linux would be worse than one that sometimes says no."""
    return any(glob_re(g).match(rel) for g in pats)

def pairignore(repo):
    """Paths excluded from the no-VCS scan, declared explicitly or not at all.

    There is deliberately no built-in deny list — no node_modules, no dist, no
    .venv. Guessing a project's build directories drops files from the review
    set silently, and silent exclusion is the one thing the review protocol
    forbids. What the scan does instead is report its own size, so a directory
    nobody meant to sweep shows up on the first command rather than at review."""
    p=Path(repo)/PAIRIGNORE
    if not p.exists(): return []
    return [t for t in (l.strip() for l in p.read_text(encoding="utf-8",errors="replace").splitlines())
            if t and not t.startswith("#")]

WILD=("*","?","[")

def prune_pats(pats):
    """Directory forms of the ignore globs, so a big subtree is never walked.

    Pruning a directory is only equivalent to filtering its contents when the
    pattern covers the *whole* subtree, so only patterns that do are turned into
    prunes: `build/**`, `build/`, and a literal path like `build` or `build/out`
    with no wildcard in it. A wildcard pattern is never a prune — `foo/*` names
    the direct children of `foo` and must not cross a segment, yet as a prune it
    matched the directory `foo/bar` and took `foo/bar/deep.txt` with it. That is
    the widening this function existed to avoid, committed by the function
    itself."""
    out=set()
    for g in pats:
        if g.endswith("/**"): out.add(g[:-3].rstrip("/")); continue
        if g.endswith("/"): out.add(g.rstrip("/")); continue
        if not any(w in g for w in WILD): out.add(g)
    return sorted(x for x in out if x)

def scan_tree(repo,pats=None):
    """Content manifest of a directory: one sorted row per file and per link.

    Rows are `(relpath, kind, size, sha256)`. The hash is over file bytes, so
    the digest this feeds is content-derived — stat metadata cannot authorize a
    boundary, because a same-size rewrite can preserve or restore an mtime and
    the next owner would then accept bytes nobody offered.

    Links — POSIX symlinks, Windows junctions, mount points — are recorded and
    never followed, and never descended into. Git supplied that containment for
    free: it stores a symlink as a blob holding its target text and never walks
    through one. A plain recursive walk would pull whatever the link points at
    into the session's review boundary from outside its root. A link's row
    hashes the target text, not the target's bytes, and carries kind `l`, so a
    link named X can never produce the same row as a regular file named X.

    **Every I/O error is fatal here.** Listing, stat, read and readlink failures
    all abort the scan by name. Tolerating them was worse than it looked: an
    unreadable directory dropped its entire subtree from the manifest, and an
    unreadable file got a fixed placeholder digest — so the failure state was
    *stable*, the digest before and after matched, and `handoff-accept` would
    authorize a transfer over bytes nothing had hashed while the documentation
    said the digest was content-derived. A boundary that cannot be computed has
    to say so. The way past a file that genuinely cannot be read is to exclude it
    in `.pairignore`, deliberately and where both roles can see it."""
    pats=pairignore(repo) if pats is None else pats
    prunes=prune_pats(pats)
    rows=[]
    def fail(what,rel,err):
        raise SystemExit(f"cannot {what} {rel!r} under {repo}: {err}\n"
                         f"  the content digest covers every file, so a path it cannot read leaves the boundary undefined;\n"
                         f"  fix the permission, or exclude the path in {PAIRIGNORE} where both roles can see it")
    def walk(d,base):
        try: entries=sorted(os.scandir(d),key=lambda e:e.name)
        except OSError as err: fail("list",base or ".",err)
        for e in entries:
            rel=f"{base}/{e.name}" if base else e.name
            # The mailbox is session state, not work under review, and `.git`
            # would be swept at any depth by a nested repository.
            if (not base and e.name==PAIR_DIR) or e.name==".git": continue
            if reparse(Path(e.path)):
                try: tgt=os.readlink(e.path)
                except OSError as err: fail("read the link at",rel,err)
                rows.append((rel,"l",-1,hashlib.sha256(tgt.encode("utf-8","surrogatepass")).hexdigest())); continue
            try: isdir=e.is_dir(follow_symlinks=False)
            except OSError as err: fail("stat",rel,err)
            if isdir:
                if not glob_match(prunes,rel): walk(e.path,rel)
                continue
            if pats and glob_match(pats,rel): continue
            try: rows.append((rel,"f",e.stat(follow_symlinks=False).st_size,
                              hashlib.sha256(Path(e.path).read_bytes()).hexdigest()))
            except OSError as err: fail("read",rel,err)
    walk(Path(repo),"")
    rows.sort()
    return rows

def manifest_blob(rows):
    r"""Manifest rows as bytes: four NUL-terminated fields per row, nothing else.

    NUL is the only separator, and rows are not newline-delimited, because NUL is
    the only byte a POSIX filename cannot contain. A newline can: `a\nb` is a
    legal name, and framing rows by newline turned one such file into two
    malformed halves that the reader then dropped — hiding the path from the
    changed set and from the digest that authorizes a handoff. Parsing is
    therefore positional, in groups of four, with no delimiter to disagree
    about."""
    return b"".join(f.encode("utf-8","surrogatepass")+b"\0" for r,k,s,d in rows for f in (r,k,str(s),d))

def tree_digest(rows): return "T:"+hashlib.sha256(manifest_blob(rows)).hexdigest()[:12]

def write_manifest(path,rows):
    path.parent.mkdir(parents=True,exist_ok=True)
    fd,tmp=tempfile.mkstemp(prefix=path.name+".",suffix=".tmp",dir=path.parent)
    try:
        with os.fdopen(fd,"wb") as f: f.write(manifest_blob(rows)); f.flush(); os.fsync(f.fileno())
        os.replace(tmp,path)
    finally:
        try: os.unlink(tmp)
        except FileNotFoundError: pass

def read_manifest(path):
    """`{relpath: (kind, size, sha256)}`, or None when there is no manifest.

    None and empty are different answers and callers rely on the difference: an
    empty manifest means the base was an empty directory, while a missing one
    means the base is unknown and the changed set has to fall back to the whole
    tree rather than to nothing.

    A manifest that is present but damaged is neither, and is refused. Skipping
    a bad record would drop a path from the base, and a path missing from the
    base compares equal to nothing — so the file would silently leave the review
    set. Loud is correct here: the fallback for an unreadable base is to rebuild
    it, which the operator can only do if told."""
    try: raw=Path(path).read_bytes()
    except (FileNotFoundError,NotADirectoryError): return None
    fields=raw.split(b"\0")
    # One trailing empty element from the final terminator, and nothing after it.
    if fields and fields[-1]==b"": fields.pop()
    if len(fields)%4: raise SystemExit(f"manifest {path} is truncated ({len(fields)} fields is not a whole number of 4-field records); the unit base cannot be trusted, so re-open the unit rather than reviewing against it")
    out={}
    for i in range(0,len(fields),4):
        rel,kind,size,dg=(f.decode("utf-8","surrogatepass") for f in fields[i:i+4])
        if rel in out: raise SystemExit(f"manifest {path} lists {rel!r} twice; the unit base is ambiguous and cannot be reviewed against")
        out[rel]=(kind,size,dg)
    return out

# A companion allowlist that matches the whole tree is not an allowlist. It turns
# the sibling into a second workspace — exactly what "read widely, write narrowly"
# exists to prevent — and it would be invisible in review, because every stray
# write would look declared.
CATCH_ALL={"**","*","**/*","./**","/**"}

def companion_root(repo,spec):
    """Resolve one declared companion to its own root, and to its own backend.

    The backend is detected per companion rather than inherited from the anchor:
    a project may well keep code in a Git repository and plan records in a plain
    directory, or the reverse, and forcing them to match would refuse a real
    layout for no safety gain. Detection is safe here in a way it is not for the
    anchor — the path is written out in `.pair-companion.json`, so there is no
    "the user was one directory above their repo" mistake to protect against."""
    q=Path(spec)
    q=(q if q.is_absolute() else (repo/q)).resolve()
    if not q.is_dir(): raise SystemExit(f"companion repository {spec!r} does not exist at {q}")
    top=git(q,"rev-parse","--show-toplevel",check=False)
    if not top: return q,"none"
    top=Path(top).resolve()
    if top!=q: raise SystemExit(f"companion repository {spec!r} points inside a working tree rather than at its root; name {top} instead")
    return top,"git"

LOCAL_BINDINGS="companions.local.json"

def load_bindings(repo):
    """This machine's paths for declared companions: `{name: path}`, or {}.

    Lives inside the mailbox directory, which is already excluded from version
    control, so a clone never inherits another machine's layout."""
    p=repo/PAIR_DIR/LOCAL_BINDINGS
    if not p.exists(): return {}
    where=f"{PAIR_DIR}/{LOCAL_BINDINGS}"
    try: cfg=json.loads(p.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e: raise SystemExit(f"{where} is not valid JSON: {e}")
    b=cfg.get("bindings") if isinstance(cfg,dict) else None
    if not isinstance(b,dict) or not all(isinstance(k,str) and isinstance(v,str) and v.strip() for k,v in b.items()):
        raise SystemExit(f'{where} must be an object with a "bindings" map of companion name to path')
    return {k.strip():v for k,v in b.items()}

def load_companions(repo):
    """The active companions; see `load_companion_decl` for the absent ones."""
    return load_companion_decl(repo)[0]

def load_companion_decl(repo):
    """Read and validate `.pair-companion.json`: `(active, absent)`.

    Declared once per anchor repository and tracked, so the exception is a
    property of the project rather than something renegotiated every session, and
    so a human reading the repository can see it too.

    An entry marked `"optional": true` may be missing on this machine. Its path
    comes from the local binding for its name, else from its tracked `"repo"`.
    When neither resolves to a directory it is returned in `absent` with a
    reason, instead of failing the start: the review boundary is still declared
    in the repository, it just has nothing on this machine to cover. A required
    entry that is missing fails exactly as before."""
    p=repo/COMPANION_FILE; bindings=load_bindings(repo)
    if not p.exists():
        if bindings: raise SystemExit(f"{PAIR_DIR}/{LOCAL_BINDINGS} binds {', '.join(sorted(bindings))} but {COMPANION_FILE} declares no companions; a binding cannot widen the boundary")
        return [],[]
    try: cfg=json.loads(p.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e: raise SystemExit(f"{COMPANION_FILE} is not valid JSON: {e}")
    if not isinstance(cfg,dict) or not isinstance(cfg.get("companions"),list):
        raise SystemExit(f'{COMPANION_FILE} must be an object with a "companions" list')
    out=[]; absent=[]; names={}; roots={}; declared=set()
    for i,c in enumerate(cfg["companions"]):
        where=f"{COMPANION_FILE} companions[{i}]"
        if not isinstance(c,dict): raise SystemExit(f"{where} must be an object")
        spec=c.get("repo"); pats=c.get("write"); why=(c.get("why") or "").strip() if isinstance(c.get("why"),str) else ""
        if "optional" in c and not isinstance(c["optional"],bool): raise SystemExit(f'{where} "optional" must be true or false')
        optional=c.get("optional") is True
        explicit=(c.get("name") or "").strip() if isinstance(c.get("name"),str) else ""
        if optional and not explicit: raise SystemExit(f'{where} is optional and needs an explicit "name"; the machine-local binding is keyed by it')
        if spec is not None and (not isinstance(spec,str) or not spec.strip()): raise SystemExit(f'{where} "repo" must be a non-empty path')
        if not optional and spec is None: raise SystemExit(f'{where} needs a "repo" path')
        if not isinstance(pats,list) or not pats or not all(isinstance(g,str) and g.strip() for g in pats):
            raise SystemExit(f'{where} needs a non-empty "write" list of path globs; an undeclared write scope is not a companion, it is a second workspace')
        # Required, and not ceremony: this string is what `status` and the
        # verification request show when a reader asks why the boundary is wider
        # than one working tree.
        if not why: raise SystemExit(f'{where} needs a "why"; the reason travels with the declaration into status and the review request')
        for g in pats:
            g2=g.strip()
            if g2 in CATCH_ALL: raise SystemExit(f"{where} glob {g!r} matches the whole companion tree; declare the record paths this session actually writes")
            if g2.startswith("/") or "\\" in g2 or (len(g2)>1 and g2[1]==":"):
                raise SystemExit(f"{where} glob {g!r} must be a relative POSIX-style pattern: forward slashes, no drive letter, no leading /")
            if ".." in PurePosixPath(g2).parts: raise SystemExit(f"{where} glob {g!r} may not escape the companion root with '..'")
        key=explicit or Path(spec).name
        # Checked before the absent branch below: two entries sharing a key make
        # both the binding and the frozen boundary ambiguous, whether each one is
        # present on this machine or not.
        if key in declared: raise SystemExit(f'{where} reuses the companion name {key!r}; give each entry a distinct "name"')
        declared.add(key)
        spec=bindings.get(key) or spec
        if optional:
            q=None if not spec else Path(spec)
            q=None if q is None else (q if q.is_absolute() else (repo/q)).resolve()
            if q is None or not q.is_dir():
                absent.append({"name":key,"reason":"unbound" if q is None else "missing","write":[g.strip() for g in pats],"why":why})
                continue
        top,cvcs=companion_root(repo,spec)
        if top==repo: raise SystemExit(f"{where} names the session's own working tree; a companion is a different repository")
        if top in repo.parents or repo in top.parents:
            raise SystemExit(f"{where} ({top}) is nested with the session tree {repo}; ownership of a shared path would be undefined")
        name=((c.get("name") or "").strip() if isinstance(c.get("name"),str) else "") or top.name
        if name in names: raise SystemExit(f'{where} reuses the companion name {name!r} (already {names[name]}); give one an explicit "name"')
        if str(top) in roots: raise SystemExit(f'{where} declares {top} twice; merge the two "write" lists')
        names[name]=str(top); roots[str(top)]=name
        # `mkey` is frozen here and never recomputed from the name, so the file a
        # companion's base manifest lives in cannot collide with the anchor's or
        # with another companion's.
        out.append({"name":name,"spec":spec,"root":str(top),"vcs":cvcs,"mkey":f"c{len(out)}","write":[g.strip() for g in pats],"why":why})
    stray=sorted(set(bindings)-declared)
    if stray: raise SystemExit(f"{PAIR_DIR}/{LOCAL_BINDINGS} binds undeclared companion(s) {', '.join(stray)}; declare them in {COMPANION_FILE} first: a binding cannot widen the boundary")
    return out,absent

def ckey(c,i):
    """A companion's frozen manifest key, or its position for one frozen before
    `mkey` existed. Position is the same value `load_companions` would have
    assigned, and the list order never changes within a session."""
    return c.get("mkey") or f"c{i}"

def companion_of(s,path):
    """The declared companion containing `path`, and the path relative to it.

    Reads the session's frozen list, never the config file. The declaration is
    resolved once at `start` and snapshotted there, so editing
    `.pair-companion.json` mid-session cannot widen what the peer is reviewing
    under them — the same reason `base_head` is a snapshot and not a live read."""
    q=Path(path).resolve()
    for c in (s.get("companions") or []):
        r=Path(c["root"])
        if q==r or r in q.parents: return c,q.relative_to(r).as_posix()
    return None,None

def snap_companions(comps,rows_out=None):
    """Base `HEAD`/status per companion, as `snap` records for the anchor tree.

    `rows_out`, when a dict is passed, receives each no-VCS companion's manifest
    rows keyed by index, so the caller writes that companion's base manifest from
    the very rows its digest was taken over rather than from a second scan."""
    out=[]
    for i,c in enumerate(comps):
        q,rows=snap_rows(Path(c["root"]),c.get("vcs"))
        if rows is not None and rows_out is not None: rows_out[i]=rows
        out.append({**c,**q})
    return out

def absent_line(c):
    return f"COMPANION_ABSENT name={c['name']} reason={c['reason']} write={','.join(c['write'])} why={c['why']}"

def companion_line(c):
    # The backend is printed, not merely stored: it tells the reader which of the
    # two boundary definitions applies to this tree — a commit range, or a
    # content digest with no history behind it.
    return (f"COMPANION {c['name']} root={c['root']} vcs={c.get('vcs') or 'git'} "
            f"write={','.join(c['write'])} why={c['why']}")

def vcs_lines(repo,s,count=None):
    """What `start` and `join` both print when the session has no version control.

    Printed rather than left in session.json, for the reason the COMPANION lines
    are: the peer's `join` prints the identical block, so both terminals learn
    the boundary — which root, how many files are in it, and what was excluded —
    without either having to be told in prose. The file count is here so that a
    directory nobody meant to sweep is visible on the first command instead of at
    review, and the ignore list is here because an exclusion that is not named is
    exactly the silent one the protocol forbids."""
    if (s.get("vcs") or mailbox_vcs(repo))!="none": return []
    pats=pairignore(repo)
    # `count` comes from the caller when it has already scanned. Re-scanning just
    # to print a number would content-hash the whole tree a second time, and
    # would also report a count taken at a different instant from the digest on
    # the line beside it.
    if count is None: count=len(scan_tree(repo,pats))
    return [f"VCS none root={repo} files={count} base={s.get('base_head')}",
            "  ownership is anchored to a sha256 content digest of this directory, not to Git HEAD;",
            "  there is no history, so the review set is a manifest delta against the unit base",
            "  ignore: "+(", ".join(pats) if pats else f"(none; add {PAIRIGNORE} to exclude paths)")]

class Lock:
    def __init__(self,p): self.p=p; self.fd=None
    def __enter__(self):
        end=time.monotonic()+10; self.p.parent.mkdir(parents=True,exist_ok=True); reaped=False
        while True:
            try:
                self.fd=os.open(self.p,os.O_CREAT|os.O_EXCL|os.O_WRONLY); os.write(self.fd,f"{os.getpid()} {now()}\n".encode()); return self
            # PermissionError is the same condition on Windows: re-creating a lock
            # file another process has just unlinked, while its delete is still
            # pending, is refused with "access denied" rather than "exists".
            # Treating it as fatal crashed concurrent posts under load.
            except (FileExistsError,PermissionError):
                t=time.monotonic()
                if t>=end:
                    # Past the deadline every path ends, bounded. A stale holder
                    # is reaped once; a lock released right at the deadline gets
                    # one short grace. A PermissionError that persists with no
                    # lock file to inspect (an unwritable directory) must fail
                    # rather than retry forever.
                    try:
                        if not reaped and time.time()-self.p.stat().st_mtime>60:
                            self.p.unlink(); reaped=True; continue
                    except FileNotFoundError:
                        if t<end+1: time.sleep(.05); continue
                    raise SystemExit("mailbox lock busy")
                time.sleep(.05)
    def __exit__(self,*_):
        if self.fd is not None: os.close(self.fd)
        try: self.p.unlink()
        except FileNotFoundError: pass

class MB:
    def __init__(self,repo): self.repo=repo; self.base=repo/PAIR_DIR; self.lock=self.base/".state.lock"
    def pointer(self):
        """The active pointer as a dict, or None. Understands the schema-2 sentinel.

        A sentinel with no schema-2 pointer is the state a crash between the two
        close steps leaves. If the session it names is finished or parked, the
        slot is free; anything else is corruption, and it is refused rather than
        guessed at."""
        p=self.base/ACTIVE_V1
        raw=read_text_retry(p)
        if raw is None: return None
        if raw.startswith(SENTINEL_PREFIX):
            # The sentinel names its session. The v2 pointer is trusted only when
            # it names the same one: a stale pointer left by an earlier session
            # must never redirect this one while an older build is blocked by a
            # sentinel naming another.
            sid=raw[len(SENTINEL_PREFIX):].split(":",1)[0].strip()
            a=readj(self.base/ACTIVE_V2)
            if isinstance(a,dict) and a.get("session_id"):
                if a["session_id"]==sid: return {**a,"schema":2}
                raise SystemExit(f"corrupt mailbox: {ACTIVE_V1} marks N-party session {sid} but {ACTIVE_V2} "
                                 f"names {a['session_id']}; refusing to guess which is live")
            d=self.base/"sessions"/sid
            t=readj(d/SESSION_V2) if re.fullmatch(r"[0-9A-Za-z-]+",sid or "") else None
            if isinstance(t,dict) and t.get("status") in (TERMINAL|{"parked"}): return None
            raise SystemExit(f"corrupt mailbox: {ACTIVE_V1} marks an N-party session ({sid or 'unknown'}) "
                             f"but {ACTIVE_V2} is missing and that session is not closed or parked")
        a=readj(p)
        return {**a,"schema":1} if isinstance(a,dict) else a
    def active(self):
        a=self.pointer()
        if not a: return None
        rec=readj(session_file(self.base/"sessions"/a["session_id"],want=a.get("schema")))
        # A live pointer whose session record is missing is corruption, not
        # "no session": reading it as absent let `start` publish a new session
        # over the pointer and orphan the one it named (spec L-8).
        if rec is None:
            raise SystemExit(f"corrupt mailbox: the pointer names session {a['session_id']}, but its record is missing; "
                             "refusing to guess. Restore the record, or remove the pointer if that session is gone")
        return check_protocol(rec,f"session {a['session_id']}")
    def point(self,s,status=None):
        """Make `s` the session the mailbox points at.

        Schema 2 writes the sentinel FIRST, then its own pointer: a crash between
        them leaves an older build already blocked, never a free-looking slot."""
        meta={"session_id":s["session_id"],"driver":s["driver"],"status":status or s["status"],"updated_at":now()}
        if schema(s)==1:
            atomic(self.base/ACTIVE_V1,meta); self._retire_v2_pointer(); return
        meta["participants"]=participants(s); meta["schema"]=2
        atomic_text(self.base/ACTIVE_V1,f"{SENTINEL_PREFIX}{s['session_id']}: this mailbox needs a pair-programming "
                    f"build that supports N-party sessions. See {ACTIVE_V2}.\n")
        atomic(self.base/ACTIVE_V2,meta)
    def unpoint(self,s):
        """Release the slot a schema-2 session held: its pointer first, then the
        sentinel, and only a sentinel that names this session. A crash between
        leaves the sentinel, which `pointer` resolves as free once `s` is closed
        or parked, while an older build stays blocked."""
        p2=self.base/ACTIVE_V2; a=readj(p2)
        if isinstance(a,dict) and a.get("session_id")==s["session_id"]:
            try: p2.unlink()
            except FileNotFoundError: pass
        p1=self.base/ACTIVE_V1
        raw=read_text_retry(p1)
        if raw is None: return
        if raw.startswith(f"{SENTINEL_PREFIX}{s['session_id']}:"):
            try: p1.unlink()
            except FileNotFoundError: pass
    def sd(self,s): return self.base/"sessions"/s["session_id"]
    def file(self,s,area,role,ext): return self.sd(s)/area/f"{role}.{ext}"
    def _retire_v2_pointer(self):
        """Remove a schema-2 pointer left behind once a schema-1 pointer is live.

        Order matters: the JSON `active.json` is already written, so the mailbox
        resolves as schema 1 whether or not this removal happens. A crash before
        it leaves a stale pointer that `pointer` ignores for a JSON `active.json`,
        and refuses to follow for any later sentinel naming another session."""
        p2=self.base/ACTIVE_V2
        try: p2.unlink()
        except FileNotFoundError: pass
    def save(self,s):
        if schema(s)==1:
            atomic(self.sd(s)/SESSION_V1,s); atomic(self.base/ACTIVE_V1,{"session_id":s["session_id"],"driver":s["driver"],"status":s["status"],"updated_at":s["updated_at"]})
            self._retire_v2_pointer()
            return
        # Schema 2: the session file first, so a crash never leaves a pointer to
        # a status the file does not hold. A session that no longer holds the
        # slot (parked or closed) releases it; one that does keeps it pointed.
        atomic(self.sd(s)/SESSION_V2,s)
        if s.get("status") in ("active","paused"): self.point(s)
        else: self.unpoint(s)
    def health(self,s,role): return self.file(s,"health",role,"json")
    def latest(self,s,role): return self.file(s,"latest",role,"json")
    def journal(self,s,role): return self.file(s,"journal",role,"jsonl")
    def cursor(self,s,role): return self.file(s,"cursor",role,"json")
    # Delivery facts (spec "Delivery facts"). Each file has one writer, under
    # that writer's role lock; none of them ever moves a cursor.
    def endpoint(self,s,role): return self.file(s,"endpoints",role,"json")
    def pushes(self,s,role): return self.file(s,"pushes",role,"jsonl")
    def receipts(self,s,role): return self.file(s,"receipts",role,"jsonl")
    def role_lock(self,s,role): return self.journal(s,role).with_suffix(".lock")
    def manifest(self,s,key="anchor",unit=None):
        """Where a no-VCS unit's base manifest lives — one file per tree per unit.

        Out of session.json on purpose: a real tree is thousands of rows, and
        session.json is rewritten on every post. Keyed by unit id so `next-unit`
        re-bases without destroying the finished unit's base, which is what a
        later audit of that unit would have to diff against.

        The key is `anchor` or `c<n>` — never a companion's display name. Naming
        the file after the name was not injective twice over: a companion the
        user called `anchor` took the anchor tree's own path, and two distinct
        roots such as `a/b` and `a_b` collapsed onto one file once the name was
        sanitized for the filesystem. Either way `start` wrote one base and a
        later review compared a different tree against it. `c<n>` is the frozen
        index of the companion in the session's own list, so it is injective by
        construction and stays stable for the life of the session."""
        if not re.fullmatch(r"anchor|c\d+",key): raise SystemExit(f"internal: manifest key {key!r} is not `anchor` or `c<n>`")
        return self.sd(s)/"manifests"/f"{key}-{unit or s.get('unit_id') or 'base'}.mf"
    def set_health(self,role,status,reason=None,resume=None,s=None):
        # `s` lets a locked caller pin the session. Re-deriving it here wrote one
        # session's health while the caller went on to mutate another.
        s=s or require(self,role,allow_paused=True); p=self.health(s,role); old=readj(p,{}) or {}
        changed=any((old.get("status")!=status,old.get("reason")!=reason,old.get("resume_at")!=resume))
        obj={"role":role,"status":status,"generation":int(old.get("generation",0))+(1 if changed or not old else 0),"updated_at":now(),"reason":reason,"resume_at":resume}; atomic(p,obj); return obj
    def post(self,role,kind,body,expect=False,expect_sid=None,to=None,reply_to=None,broadcast=False,internal=False,forward=False):
        s=require(self,role,allow_paused=True)
        if expect_sid and s["session_id"]!=expect_sid:
            raise SystemExit(f"session changed under this command (expected {expect_sid}, active {s['session_id']})")
        # Checked on THIS snapshot, the one that chooses the journal. A check made
        # by the caller on an earlier read could validate one session and let the
        # append land in a replacement session with different participants.
        if kind not in KINDS: raise SystemExit("invalid kind")
        fields=address(self,s,role,kind,expect,to,reply_to,broadcast,internal,forward)
        if not body.strip(): raise SystemExit("refusing to post an empty message; a body that failed to render is worse than no post, because the peer treats it as a real turn")
        if len(body)>12000: raise SystemExit("message too large; reference a repo artifact instead")
        # A post by the role that owns the pause is not a claim that the peer is
        # back — it IS the peer executing, which is the only evidence that ever
        # mattered. The whole PAUSE RECORD is captured, not just its role, so a
        # post begun under one pause cannot clear a newer same-role replacement.
        entry_pause=pauses(s).get(role)
        if entry_pause:
            # Persisted BEFORE the append. Reaching this line is already the
            # proof, so a failed append may lose the message but must not leave
            # a pause the evidence has already disproved — which is precisely the
            # stale state this whole change exists to remove.
            with Lock(self.lock):
                fresh=self.active()
                if (fresh and fresh["session_id"]==s["session_id"]
                        and pauses(fresh).get(role)==entry_pause):
                    # Session first. It is the authoritative record and the one
                    # that blocks work, so a failure here leaves the pre-recovery
                    # state intact and retryable. Writing health first instead
                    # left health=ready beside a surviving pause — the disproved
                    # pause standing, which is the whole defect.
                    clear_pause(fresh,role); fresh["updated_at"]=now(); self.save(fresh)
                    self.set_health(role,"ready",s=fresh)
        p=self.journal(s,role); p.parent.mkdir(parents=True,exist_ok=True)
        # One writer at a time per role journal. Allocating the sequence and
        # appending outside a lock let two concurrent posts by the same role take
        # the same number, and on Windows interleave their appends into a torn
        # line. Found by a concurrency stress run; a per-role lock, not the
        # state lock, so the peer's posts are never serialised behind this one.
        with Lock(p.with_suffix(".lock")):
            # The next seq follows whichever is higher: `latest`, or the journal's
            # own last record. `latest` is written after the append, so a crash
            # between the two left it one behind, and the next post reused the
            # seq, giving two messages one msg_id.
            last=readj(self.latest(s,role)); head=journal_records(p)[-1:] if p.exists() else []
            seq=max([int(last.get("seq",0)) if last else 0]+[int(x.get("seq",0)) for x in head])+1
            seal_torn_tail(p)
            m={"seq":seq,"at":now(),"role":role,"kind":kind,"work_unit":s["work_unit"],"unit_id":s.get("unit_id"),"expect_reply":bool(expect),"body":body.strip()}
            # The owner as this message's own snapshot saw it. The append lands
            # outside the session lock, so a message begun before a handoff can
            # arrive after it; the stamp, not the arrival, says whether its
            # author wrote it as the owner.
            if schema(s)==2: m["owner"]=s["owner"]
            # Each writer states the protocol on its first record, so a reader of one
            # journal alone knows the rules it was written under.
            if seq==1: m["protocol"]=PROTOCOL
            if fields:
                mid=f"{role}:{seq}"
                m.update({"msg_id":mid,"to":fields["to"],"reply_to":fields["reply_to"],"broadcast":fields["broadcast"],
                          "thread_id":fields["thread_id"] or mid,"route_trace":fields["route_trace"],"ttl":fields["ttl"],
                          "forward":bool(fields.get("forward"))})
            with p.open("a",encoding="utf-8",newline="\n") as f: f.write(journal_record(m)); f.flush(); os.fsync(f.fileno())
            atomic(self.latest(s,role),m)
        with Lock(self.lock):
            fresh=self.active()
            if fresh and fresh["session_id"]==s["session_id"]:
                # The inverse drift the session-first ordering can produce:
                # session recovered, health write interrupted. Reconciled from
                # this FRESH read, not the entry snapshot — a rate limit landing
                # after require() would otherwise let a stale "active" reading
                # overwrite the new pause's health while the pause survived.
                if not entry_pause and role not in pauses(fresh):
                    h=readj(self.health(fresh,role),{}) or {}
                    if h.get("status") in ("rate_limited","paused","offline"):
                        self.set_health(role,"ready",s=fresh)
                # `waiting` belongs to the current work unit. This message was
                # stamped under the unit read at entry, and the append happens
                # outside the lock, so a post begun before a boundary can arrive
                # after it — clearing a wait that belongs to the new unit, or
                # installing an expected reply for a unit that is over. Health
                # recovery above is session-level and stays unconditional.
                if fresh.get("unit_id")==m.get("unit_id"):
                    if schema(fresh)==1:
                        w=fresh.get("waiting")
                        if w and w.get("for_role")==role: fresh["waiting"]=None
                        if expect: fresh["waiting"]={"from_role":role,"for_role":the_peer(fresh,role),"seq":seq,"kind":kind,"since":m["at"]}
                    else:
                        wm=waiting_map(fresh)
                        # Only a real reply answers: a forward delegates, and must
                        # not tell the asker that its recipient has answered.
                        rt=m.get("reply_to")
                        # An escalation is a notice to the thread origin, not an
                        # answer either.
                        if rt in wm and not m.get("forward") and kind!="ESCALATE" and role in wm[rt]["pending"]:
                            wm[rt]["pending"]=[x for x in wm[rt]["pending"] if x!=role]
                            wm[rt]["answered_by"]=wm[rt].get("answered_by",[])+[role]
                            if not wm[rt]["pending"]: del wm[rt]
                        if expect: wm[m["msg_id"]]={"from":role,"kind":kind,"since":m["at"],"pending":list(m["to"]),"answered_by":[]}
                        fresh["waiting"]=wm or None
                fresh["updated_at"]=now(); self.save(fresh)
        return m

def require(mb,role,allow_paused=False):
    s=mb.active()
    if not s: raise SystemExit("NO_ACTIVE_SESSION")
    if role not in participants(s):
        raise SystemExit(f"role not in active session (participants: {', '.join(participants(s))})")
    if s["status"] in ("completed","abandoned"): raise SystemExit("SESSION_CLOSED")
    if s["status"]=="parked": raise SystemExit("SESSION_PARKED")
    if s["status"]=="paused" and not allow_paused: raise SystemExit("SESSION_PAUSED")
    return s

def body(a):
    if getattr(a,"body_file",None): return Path(a.body_file).read_text(encoding="utf-8")
    if getattr(a,"body",None) is not None: return a.body
    raise SystemExit("body required")
def slug(s): return (re.sub(r"[^a-z0-9]+","-",s.lower()).strip("-")[:48] or "pair-task")

def start_participants(role,with_):
    """The ordered participant list `start` records: the starter first.

    No `--with` means the historic pair, so today's two-party command lines are
    unchanged. LocalPilot has no historic peer and must name its partners."""
    if not with_:
        if role not in HISTORIC:
            raise SystemExit(f"--role {role} has no default partner; name the other participant(s) with --with")
        return [role,[r for r in HISTORIC if r!=role][0]]
    named=[x.strip() for x in with_.split(",") if x.strip()]
    bad=[x for x in named if x not in PARTICIPANTS]
    if bad: raise SystemExit(f"unknown participant(s) {', '.join(bad)}; choose from {', '.join(PARTICIPANTS)}")
    if len(set(named))!=len(named): raise SystemExit("--with names a participant twice")
    parts=[role]+[x for x in named if x!=role]
    if len(parts)!=len(named)+1: raise SystemExit("--with must not repeat the starting role")
    if not 2<=len(parts)<=3: raise SystemExit(f"a session has 2 or 3 participants, not {len(parts)}")
    return parts

def start(a):
    repo=root(a.repo,no_vcs=getattr(a,"no_vcs",False)); mb=MB(repo); task=body(a).strip()
    # An existing mailbox's recorded backend wins over the flag: `root` has
    # already refused the two ways they could genuinely disagree, so what is left
    # is a second `start` in a directory whose mode was settled by the first.
    vcs="none" if (getattr(a,"no_vcs",False) or ((repo/PAIR_DIR).is_dir() and mailbox_vcs(repo)=="none")) else "git"
    # Outside the lock: reading the declaration touches no mailbox state, and a
    # malformed one should fail without having taken a lock other terminals wait on.
    comps,absent=load_companion_decl(repo)
    parts=start_participants(a.role,getattr(a,"with_",None))
    auth=declare_authority(parts,a.role,getattr(a,"advisers",None))
    with Lock(mb.lock):
        # Inside the lock. Outside it, `start` could read info/exclude, see the
        # rule and skip, while a concurrent `purge` removed that same rule under
        # the lock - leaving a live mailbox that is no longer ignored.
        exclude(repo,vcs)
        old=mb.active()
        if old and old["status"] not in ("completed","abandoned","parked"):
            print(f"ACTIVE session={old['session_id']} driver={old['driver']} status={old['status']} work={old['work_unit']}"); return 3
        sid=datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")+"-"+uuid.uuid4().hex[:8]
        # Before anything reads it back. `mailbox_vcs` is the authority every
        # later command consults, including the peer's very first `join`.
        atomic(mb.base/VCS_FILE,{"vcs":vcs,"recorded_at":now()})
        # One scan per tree, anchor and companions alike: `q`/`rows` and
        # `crows` are what both the digests and the stored manifests come from.
        q,rows=snap_rows(repo,vcs); crows={}
        s={"session_id":sid,"task":task,"work_unit":a.work_unit or slug(task),"unit_id":"1-"+uuid.uuid4().hex[:8],"driver":a.role,"navigator":parts[1],"owner":a.role,"ownership_epoch":1,"status":"active","phase":"huddle","waiting":None,"pause":None,"handoff":None,"vcs":vcs,"base_head":q["head"],"base_status":q["status"],"companions":snap_companions(comps,crows),"absent_companions":absent,"caps":{a.role:list(CAPS)},"delivery":"print","protocol":PROTOCOL,"created_at":now(),"updated_at":now()}
        if set(parts)!=set(HISTORIC):
            # Schema 2 carries its participant list and no single navigator: with
            # three identities there is none, and a derived one would be a guess.
            s.pop("navigator"); s.pop("pause"); s["schema"]=2; s["participants"]=parts; s["authority"]=auth; s["pauses"]={}
        for area in ("journal","latest","health","cursor"): (mb.sd(s)/area).mkdir(parents=True,exist_ok=True)
        if rows is not None: write_manifest(mb.manifest(s),rows)
        # Not gated on the anchor's mode: a Git anchor may declare a no-VCS
        # companion, and that companion still needs a base to be reviewed
        # against. Gating it left `changed_paths` with no manifest there, falling
        # back to the whole tree on every request.
        for i,r in crows.items(): write_manifest(mb.manifest(s,ckey(s["companions"][i],i)),r)
        mb.save(s)
        for r in participants(s):
            atomic(mb.health(s,r),{"role":r,"status":"ready" if r==a.role else "not_joined","generation":1,"updated_at":now(),"reason":None,"resume_at":None})
            atomic(mb.cursor(s,r),v2_cursor(s,r,{}) if schema(s)==2 else {"peer_seq":0,"peer_health_generation":0,"resume_seen":None,"stale_wait_seq":None})
    others=[r for r in participants(s) if r!=a.role]
    # Two participants keep the historic line exactly; three name every peer.
    print(f"STARTED session={sid} role=driver peer={others[0]} work={s['work_unit']}" if len(others)==1 else
          f"STARTED session={sid} role=driver peers={','.join(others)} work={s['work_unit']}")
    if hasattr(a,"anchor_source"): print(anchor_line(a))
    for line in vcs_lines(repo,s,None if rows is None else len(rows)): print(line)
    # Printed rather than left in session.json: the navigator's `join` prints the
    # same lines, so both terminals learn the widened boundary without either
    # having to be told about it in prose.
    for c in s["companions"]: print(companion_line(c))
    for c in s["absent_companions"]: print(absent_line(c))
    return 0

def join(a):
    repo=root(a.repo); mb=MB(repo); end=None if a.timeout==0 else time.monotonic()+a.timeout
    while True:
        s=mb.active()
        if s and s["status"]=="parked": raise SystemExit(f"SESSION_PARKED session={s['session_id']} work={s['work_unit']}; use `resume` to bring it back")
        if s and s["status"] not in ("completed","abandoned"):
            # Announce a real transition into ready — a first join, or a return
            # from a quota pause — but stay silent when an already-ready peer
            # merely re-invokes the skill. Every HELLO is a message the driver
            # has to read past.
            # Read the session, record readiness and unpause it under one lock,
            # and carry that session forward. Doing these separately let a
            # park+start land between them, so readiness and the unpause could
            # describe the replacement while the printed task came from the one
            # this join actually resolved.
            with Lock(mb.lock):
                s=mb.active()
                if not s or s["status"] in ("completed","abandoned","parked"): continue
                # Before any write: a refused join must leave the mailbox exactly
                # as it found it (no health file, no capability, no save).
                if a.role not in participants(s):
                    raise SystemExit(f"role not in active session (participants: {', '.join(participants(s))})")
                was=(readj(mb.health(s,a.role),{}) or {}).get("status")
                # Session first, for the same reason as post: it is the record
                # that blocks work, so an interruption leaves the pre-recovery
                # state whole instead of health=ready beside a surviving pause.
                if a.role in pauses(s):
                    clear_pause(s,a.role); s["updated_at"]=now(); mb.save(s)
                # Advertise, and upgrade to acknowledged delivery only when both
                # roles have. Under this lock and before the HELLO below, so the
                # first message the pair exchanges is already delivered under
                # the mode both ends will honour. A session whose starter never
                # advertised stays on print delivery for its whole life.
                caps=s.setdefault("caps",{}); changed=caps.get(a.role)!=CAPS
                caps[a.role]=list(CAPS)
                if delivery(s)=="print" and all("ack" in (caps.get(r) or []) for r in participants(s)):
                    s["delivery"]="ack"; changed=True
                if changed: s["updated_at"]=now(); mb.save(s)
                mb.set_health(a.role,"ready",s=s)
            # post takes the lock itself, so it happens outside — pinned to the
            # session id resolved above rather than to whatever is current.
            if a.role!=s["driver"] and was!="ready":
                mb.post(a.role,"HELLO","joined; navigator ready",expect_sid=s["session_id"],
                        broadcast=schema(s)==2,internal=True)
            # The lock is released by now, so re-confirm before announcing a task.
            # This covers the ready/no-HELLO path too, which posts nothing and
            # would otherwise print a task that is no longer the active one.
            cur=mb.active()
            if not cur or cur["session_id"]!=s["session_id"]: continue
            peers=[r for r in participants(s) if r!=a.role]
            rl='driver' if a.role==s['driver'] else 'navigator'
            print(f"JOINED session={s['session_id']} role={rl} peer={peers[0]} work={s['work_unit']}" if len(peers)==1 else
                  f"JOINED session={s['session_id']} role={rl} peers={','.join(peers)} work={s['work_unit']}")
            if hasattr(a,"anchor_source"): print(anchor_line(a))
            for line in vcs_lines(repo,s): print(line)
            # Before the task, not after. A navigator that learns the review
            # boundary only by finding the config file may never find it, and a
            # companion nobody reviews is the failure this exists to prevent.
            for c in (s.get("companions") or []): print(companion_line(c))
            for c in (s.get("absent_companions") or []): print(absent_line(c))
            print(f"TASK:\n{s['task']}"); return 0
        if end is not None and time.monotonic()>=end: return 1
        time.sleep(a.poll)

def unread(mb,s,role,c):
    p=mb.journal(s,the_peer(s,role)); after=int(c.get("peer_seq",0)); out=[]
    if p.exists():
        out=[m for m in journal_records(p) if int(m.get("seq",0))>after]
    return out

def delivery(s):
    """`ack` or `print`. A session predating negotiation has no field: print."""
    return s.get("delivery") or "print"

def parse_ack(s,role,through):
    """`{sender: seq}` from an --through / --ack-through value.

    A bare number names the one other participant, so it is accepted only when
    there is exactly one. `sender:N[,sender:N]` names each sender explicitly; a
    sender given twice, an unknown sender, or the reader itself is refused."""
    others=[r for r in participants(s) if r!=role]
    t=str(through).strip()
    if re.fullmatch(r"[1-9][0-9]*",t):
        if len(others)!=1:
            raise SystemExit(f"a bare --through names one sender; with {len(participants(s))} participants "
                             f"write sender:N[,sender:N] (senders: {', '.join(others)})")
        return {others[0]:int(t)}
    if schema(s)==1: raise SystemExit("sender:N acknowledgement needs an N-party session")
    vec={}
    for part in t.split(","):
        m=re.fullmatch(r"([a-z]+):([1-9][0-9]*)",part.strip())
        if not m: raise SystemExit(f"cannot read acknowledgement {part!r}; expected sender:N")
        snd,n=m.group(1),int(m.group(2))
        if snd==role: raise SystemExit("a participant does not acknowledge its own mail")
        if snd not in others: raise SystemExit(f"unknown sender {snd!r} (senders: {', '.join(others)})")
        if snd in vec: raise SystemExit(f"{snd} is named twice in the acknowledgement")
        vec[snd]=n
    return vec

def apply_ack(mb,s,role,through):
    """Advance `role`'s acknowledged cursor(s). Caller holds the lock.

    Only mail already printed to this role can be acknowledged: acking past
    `delivered_seq` would mark as read a message no reader was ever shown, which
    is the defect acknowledged delivery exists to remove."""
    vec=parse_ack(s,role,through)
    if schema(s)==2: return apply_ack_v2(mb,s,role,vec)
    through=next(iter(vec.values()))
    if delivery(s)!="ack": return f"ACK_NOT_NEEDED delivery={delivery(s)}"
    p=mb.cursor(s,role); c=readj(p,{}) or {}
    acked=int(c.get("peer_seq",0)); dl=max(acked,int(c.get("delivered_seq",0)))
    if through>dl:
        raise SystemExit(f"cannot ack #{through}: only #{dl} has been delivered to {role}"
                         +(f"; valid range #{acked+1}..#{dl}" if dl>acked else "; nothing delivered is unacknowledged"))
    if through<=acked: return f"ACKED through={acked} (already)"
    c["peer_seq"]=through
    c["delivered_counts"]={k:v for k,v in (c.get("delivered_counts") or {}).items() if int(k)>through}
    atomic(p,c); return f"ACKED through={through}"

SCALAR_MAIL=("peer_seq","delivered_seq","delivered_counts")
SCALAR_HEALTH={"peer_health_generation":"gen","last_actionable_health":"last","resume_seen":"resume_seen"}

def v2_cursor(s,role,c):
    """A schema-2 cursor in its one shape: {from, health, stale_marks}.

    Every reader goes through here, so a cursor written in the older scalar
    shape (a schema-2 pair created before per-sender cursors) is migrated
    once, the same way, whichever command touches it first, and no scalar
    mail or health field survives next to its per-sender replacement."""
    others=[r for r in participants(s) if r!=role]
    c=dict(c or {}); frm=c.setdefault("from",{}); hs=c.setdefault("health",{}); c.setdefault("stale_marks",[])
    legacy=any(k in c for k in SCALAR_MAIL) or any(k in c for k in SCALAR_HEALTH)
    if legacy and len(others)==1:
        peer=others[0]; st=frm.setdefault(peer,{"peer_seq":0,"delivered_seq":0,"delivered_counts":{}})
        st["peer_seq"]=max(int(st.get("peer_seq",0)),int(c.get("peer_seq") or 0))
        st["delivered_seq"]=max(int(st.get("delivered_seq",0)),int(c.get("delivered_seq") or 0))
        st["delivered_counts"]={**dict(c.get("delivered_counts") or {}),**dict(st.get("delivered_counts") or {})}
        h=hs.setdefault(peer,{})
        for old,new in SCALAR_HEALTH.items():
            if c.get(old) is not None and h.get(new) is None: h[new]=c[old]
        if "gen" in h: h["gen"]=int(h["gen"] or 0)
    # `stale_wait_seq` was the one-wait marker; schema 2 keeps one marker per
    # (wait, silent recipient) in `stale_marks` instead.
    for k in (*SCALAR_MAIL,*SCALAR_HEALTH,"stale_wait_seq"): c.pop(k,None)
    for snd in others:
        st=frm.setdefault(snd,{"peer_seq":0,"delivered_seq":0,"delivered_counts":{}})
        st.setdefault("peer_seq",0); st.setdefault("delivered_seq",0); st.setdefault("delivered_counts",{})
    return c

def apply_ack_v2(mb,s,role,vec):
    """All-or-nothing: every component is validated before any is applied."""
    if delivery(s)!="ack": return f"ACK_NOT_NEEDED delivery={delivery(s)}"
    p=mb.cursor(s,role); raw=readj(p,{}) or {}; c=v2_cursor(s,role,raw); frm=c["from"]
    plan=[]
    for snd,n in vec.items():
        # Existence and visibility first, before any "already acknowledged"
        # shortcut: a cursor that passed over unaddressed mail must not make an
        # explicit acknowledgement of that mail look valid.
        m=find_msg(mb,s,f"{snd}:{n}")
        if not m: raise SystemExit(f"cannot ack {snd}:{n}: there is no such message")
        if not visible(m,role): raise SystemExit(f"cannot ack {snd}:{n}: it is not mail addressed to {role}")
        st=frm.get(snd) or {}; acked=int(st.get("peer_seq",0)); dl=max(acked,int(st.get("delivered_seq",0)))
        if n<=acked: continue
        if n>dl: raise SystemExit(f"cannot ack {snd}:{n}: only {snd}:{dl} has been delivered to {role}")
        plan.append((snd,n))
    if not plan:
        # Nothing new to acknowledge, but a cursor migrated from the older shape
        # is still written back: this command succeeded, and the next reader
        # should not find the scalar fields again.
        if c!=raw: atomic(p,c)
        return "ACKED (already) "+",".join(f"{k}:{v}" for k,v in vec.items())
    for snd,n in plan:
        st=frm.setdefault(snd,{"peer_seq":0,"delivered_seq":0,"delivered_counts":{}})
        st["peer_seq"]=n
        st["delivered_counts"]={k:v for k,v in (st.get("delivered_counts") or {}).items() if int(k)>n}
    atomic(p,c); return "ACKED "+",".join(f"{snd}:{n}" for snd,n in plan)

def unacked_v2(mb,s,role):
    """`[(sender, first, last)]` of mail printed to `role` but not acknowledged."""
    if delivery(s)!="ack": return []
    frm=v2_cursor(s,role,readj(mb.cursor(s,role),{}) or {})["from"]
    return [(snd,int(st.get("peer_seq",0))+1,int(st.get("delivered_seq",0))) for snd,st in sorted(frm.items())
            if int(st.get("delivered_seq",0))>int(st.get("peer_seq",0))]

def unacked_range(mb,s,role):
    """`(first, last)` of mail printed to `role` but not acknowledged, or None."""
    if delivery(s)!="ack": return None
    c=readj(mb.cursor(s,role),{}) or {}; acked=int(c.get("peer_seq",0)); dl=int(c.get("delivered_seq",0))
    return (acked+1,dl) if dl>acked else None

def consume(mb,role,stale,expect_sid=None):
    """Return `(message_or_None, commit)`; `commit()` persists the read cursor.

    The cursor is deliberately NOT advanced in here. A caller that cannot deliver
    the message — a broken pipe, an encoder that rejects a character — must be
    able to leave it unread so the next `watch` redelivers it. Advancing first
    dropped peer mail permanently on any output failure, recoverable only from
    `transcript`.

    Under `ack` delivery the read cursor is never advanced here at all, even on
    a successful print: printing proves only that stdout took the bytes, not that
    a model read them, and an orphaned watcher once swallowed peer mail that way.
    The commit records the delivery instead, and the message keeps coming back,
    marked REDELIVERED, until the role acknowledges it (`ack`, or `--ack-through`).
    """
    s=require(mb,role,allow_paused=True)
    if expect_sid and s["session_id"]!=expect_sid: raise SystemExit(f"SESSION_SWITCHED expected={expect_sid} active={s['session_id']}")
    if schema(s)==2: return consume_v2(mb,s,role,stale,expect_sid)
    acking=delivery(s)=="ack"
    c=readj(mb.cursor(s,role),{}) or {}; entry=json.loads(json.dumps(c)); msgs=unread(mb,s,role,c)
    def commit():
        if not acking: atomic(mb.cursor(s,role),c); return
        if c==entry: return
        # Merged under the lock, so an `ack` landing between this read and this
        # write is kept rather than overwritten by the stale snapshot.
        with Lock(mb.lock):
            cur=readj(mb.cursor(s,role),{}) or {}
            c["peer_seq"]=max(int(cur.get("peer_seq",0)),int(c.get("peer_seq",0)))
            c["delivered_seq"]=max(int(cur.get("delivered_seq",0)),int(c.get("delivered_seq",0)))
            counts={k:max(int(v),int((c.get("delivered_counts") or {}).get(k,0))) for k,v in (cur.get("delivered_counts") or {}).items()}
            counts.update({k:v for k,v in (c.get("delivered_counts") or {}).items() if k not in counts})
            c["delivered_counts"]={k:v for k,v in counts.items() if int(k)>c["peer_seq"]}
            atomic(mb.cursor(s,role),c)
    if msgs:
        n={}
        if acking:
            counts=c.setdefault("delivered_counts",{})
            for m in msgs: n[m["seq"]]=int(counts.get(str(m["seq"]),0))+1; counts[str(m["seq"])]=n[m["seq"]]
            c["delivered_seq"]=max(int(c.get("delivered_seq",0)),max(n))
        else:
            c["peer_seq"]=max(m["seq"] for m in msgs)
        # The unit goes in the live heading for the same reason it goes in the
        # transcript: a message stamped under the previous unit can be delivered
        # during this one, and without the label it reads as current.
        def head(m):
            u=f" [{unit_label(m.get('work_unit'),m.get('unit_id'))}]" if m.get("work_unit") else ""
            r=f" REDELIVERED n={n[m['seq']]}" if n.get(m["seq"],0)>1 else ""
            return f"PEER {m['role']} #{m['seq']} {m['kind']}{u}{' REPLY_REQUIRED' if m.get('expect_reply') else ''}{r}"
        return "\n\n".join(head(m)+("\n"+m['body'] if m.get('body') else "") for m in msgs), commit
    peer=the_peer(s,role)
    h=readj(mb.health(s,peer),{}) or {}; gen=int(h.get("generation",0)); seen=int(c.get("peer_health_generation",0)); status=h.get("status")
    if gen>seen:
        c["peer_health_generation"]=gen
        if status in ("rate_limited","paused","offline"):
            c["last_actionable_health"]=status; return f"PEER_HEALTH {peer} status={status} resume_at={h.get('resume_at') or 'unknown'} reason={h.get('reason') or 'unspecified'}", commit
        if status=="ready" and c.get("last_actionable_health") in ("rate_limited","paused","offline"):
            c["last_actionable_health"]="ready"; return f"PEER_HEALTH {peer} status=ready", commit
    if status=="rate_limited" and h.get("resume_at"):
        due=parse_ts(h["resume_at"])
        if due and time.time()>=due and c.get("resume_seen")!=h["resume_at"]:
            c["resume_seen"]=h["resume_at"]; return f"PEER_RESUME_DUE {peer} reset={h['resume_at']} (retry due; access not proven)", commit
    fresh=mb.active() or s
    if expect_sid and fresh["session_id"]!=expect_sid: raise SystemExit(f"SESSION_SWITCHED expected={expect_sid} active={fresh['session_id']}")
    s=fresh; w=s.get("waiting")
    if w and w.get("from_role")==role and c.get("stale_wait_seq")!=w.get("seq"):
        since=datetime.fromisoformat(w["since"].replace("Z","+00:00")).timestamp()
        if time.time()-since>=stale and status!="rate_limited": c["stale_wait_seq"]=w["seq"]; return f"PEER_STALE {peer} waiting_on={w['kind']}#{w['seq']} last_health={h.get('updated_at') or 'unknown'}", commit
    return None, commit

def route_marks(m):
    """The directed-mail suffix of a schema-2 heading: recipients, id, lineage."""
    out=f" -> {','.join(m.get('to') or [])} id={m.get('msg_id')}"
    if m.get("reply_to"): out+=f" re={m['reply_to']}"
    if m.get("forward"): out+=" fwd"
    if m.get("broadcast"): out+=" broadcast"
    return out

def journal_after(mb,s,sender,after):
    p=mb.journal(s,sender); out=[]
    if p.exists():
        out=[m for m in journal_records(p) if int(m.get("seq",0))>after]
    return sorted(out,key=lambda m:int(m.get("seq",0)))

def consume_v2(mb,s,role,stale,expect_sid):
    """`consume` for schema 2: one cursor per sender, visible mail only.

    Mail not addressed to this role is never shown and never needs an
    acknowledgement: each sender's cursor moves across the unaddressed messages
    at its head, and stops before the first addressed message still unacked.
    Under print delivery printing advances the cursor, as in schema 1."""
    acking=delivery(s)=="ack"
    others=[r for r in participants(s) if r!=role]
    raw=readj(mb.cursor(s,role),{}) or {}; c=v2_cursor(s,role,raw); entry=json.loads(json.dumps(raw))
    frm=c["from"]
    out=[]; n={}
    for snd in others:
        st=frm.setdefault(snd,{"peer_seq":0,"delivered_seq":0,"delivered_counts":{}})
        head=True
        for m in journal_after(mb,s,snd,int(st.get("peer_seq",0))):
            if not visible(m,role):
                if head or not acking: st["peer_seq"]=int(m["seq"])
                continue
            if acking:
                head=False
                k=str(m["seq"]); cnt=int(st["delivered_counts"].get(k,0))+1
                st["delivered_counts"][k]=cnt; n[(snd,int(m["seq"]))]=cnt
                st["delivered_seq"]=max(int(st.get("delivered_seq",0)),int(m["seq"]))
            else:
                st["peer_seq"]=int(m["seq"])
            out.append(m)
    def commit():
        if c==entry: return
        with Lock(mb.lock):
            cur=readj(mb.cursor(s,role),{}) or {}
            cf=cur.get("from") or {}
            for snd,st in frm.items():
                o=cf.get(snd) or {}
                st["peer_seq"]=max(int(o.get("peer_seq",0)),int(st.get("peer_seq",0)))
                st["delivered_seq"]=max(int(o.get("delivered_seq",0)),int(st.get("delivered_seq",0)))
                counts={k:max(int(v),int(st["delivered_counts"].get(k,0))) for k,v in (o.get("delivered_counts") or {}).items()}
                counts.update({k:v for k,v in st["delivered_counts"].items() if k not in counts})
                st["delivered_counts"]={k:v for k,v in counts.items() if int(k)>st["peer_seq"]}
            # Health and stale markers are merged from the locked current cursor
            # too: a newer observation by another consumer must survive this
            # (older) snapshot. The higher generation wins; at an equal one,
            # neither side's recorded fields are dropped.
            ch=v2_cursor(s,role,cur).get("health") or {}
            for snd,o in ch.items():
                mine=c["health"].get(snd)
                if not mine or int(o.get("gen",0))>int(mine.get("gen",0)): c["health"][snd]=dict(o)
                elif int(o.get("gen",0))==int(mine.get("gen",0)):
                    for k,v in o.items():
                        if mine.get(k) is None: mine[k]=v
            c["stale_marks"]=sorted(set(c.get("stale_marks") or [])|set(cur.get("stale_marks") or []))
            atomic(mb.cursor(s,role),c)
    if out:
        out.sort(key=lambda m:(m.get("at",""),m.get("role",""),int(m.get("seq",0))))
        def head_line(m):
            u=f" [{unit_label(m.get('work_unit'),m.get('unit_id'))}]" if m.get("work_unit") else ""
            cnt=n.get((m["role"],int(m["seq"])),0)
            rd=f" REDELIVERED n={cnt}" if cnt>1 else ""
            # With one peer there is nothing to disambiguate, so a schema-2 pair
            # keeps the historic heading. In a triad the reader needs to see who
            # else got it and which id to --reply-to or acknowledge.
            mk=route_marks(m) if len(others)>1 else ""
            return f"PEER {m['role']} #{m['seq']} {m['kind']}{u}{mk}{' REPLY_REQUIRED' if m.get('expect_reply') else ''}{rd}"
        return "\n\n".join(head_line(m)+("\n"+m['body'] if m.get('body') else "") for m in out), commit
    # Health and resume notices, per sender.
    hs=c["health"]
    for snd in others:
        h=readj(mb.health(s,snd),{}) or {}; gen=int(h.get("generation",0)); hc=hs.setdefault(snd,{})
        status=h.get("status")
        if gen>int(hc.get("gen",0)):
            hc["gen"]=gen
            if status in ("rate_limited","paused","offline"):
                hc["last"]=status; return f"PEER_HEALTH {snd} status={status} resume_at={h.get('resume_at') or 'unknown'} reason={h.get('reason') or 'unspecified'}", commit
            if status=="ready" and hc.get("last") in ("rate_limited","paused","offline"):
                hc["last"]="ready"; return f"PEER_HEALTH {snd} status=ready", commit
        if status=="rate_limited" and h.get("resume_at"):
            due=parse_ts(h["resume_at"])
            if due and time.time()>=due and hc.get("resume_seen")!=h["resume_at"]:
                hc["resume_seen"]=h["resume_at"]; return f"PEER_RESUME_DUE {snd} reset={h['resume_at']} (retry due; access not proven)", commit
    # Stale waits: each of this role's waits, each silent recipient on its own
    # clock and its own once-only marker.
    fresh=mb.active() or s
    if expect_sid and fresh["session_id"]!=expect_sid: raise SystemExit(f"SESSION_SWITCHED expected={expect_sid} active={fresh['session_id']}")
    marks=c.setdefault("stale_marks",[])
    for mid,e in sorted(waiting_map(fresh).items()):
        if e.get("from")!=role: continue
        since=datetime.fromisoformat(e["since"].replace("Z","+00:00")).timestamp()
        if time.time()-since<stale: continue
        for rcp in e.get("pending") or []:
            key=f"{mid}>{rcp}"; h=readj(mb.health(fresh,rcp),{}) or {}
            if key in marks or h.get("status")=="rate_limited": continue
            marks.append(key)
            return f"PEER_STALE {rcp} waiting_on={e['kind']}#{mid} last_health={h.get('updated_at') or 'unknown'}", commit
    return None, commit

def emit(text):
    """Write one delivery to stdout and prove it landed before the caller commits."""
    print(text); sys.stdout.flush()

def watch(a,block=True):
    mb=MB(root(a.repo)); end=None if not block or a.timeout==0 else time.monotonic()+a.timeout
    # A blocking watch belongs to the session it started on. Without this a fast
    # park+start hands the waiting navigator the NEW task's mail while it is
    # still holding the old task in context, with no join and no HELLO.
    start_sid=(mb.active() or {}).get("session_id")
    # "Acknowledge, then re-arm" as one command, so the skill's loop never has a
    # window where processed mail is still unacknowledged and redelivered.
    if getattr(a,"ack_through",None) is not None:
        with Lock(mb.lock): apply_ack(mb,require(mb,a.role,allow_paused=True),a.role,a.ack_through)
    while True:
        out,commit=consume(mb,a.role,a.stale_after,expect_sid=start_sid)
        if out: emit(out); commit(); return 0
        commit()
        if not block: return 0
        if end is not None and time.monotonic()>=end: return 1
        time.sleep(a.poll)

def health(a):
    mb=MB(root(a.repo)); resume=fmt_ts(parse_ts(a.resume_at)) if a.resume_at else None
    # The health file and the pause it triggers must describe the same session.
    # Writing health first, unlocked, let a park+start land in between and pause
    # the replacement on the strength of the old session's transition.
    with Lock(mb.lock):
        s=require(mb,a.role,allow_paused=True)
        # Recovery is never declared, only demonstrated. Clearing a pause from
        # here would let either terminal restore its own writes by asserting the
        # peer is back — the automatic solo fallback the pause exists to prevent,
        # and `--role` is a protocol claim, not authentication. Reporting a peer
        # DOWN from evidence stays legal; reporting one BACK does not.
        if a.status=="ready" and a.role in pauses(s):
            raise SystemExit(f"{a.role} owns the recorded pause; it clears when that terminal runs `join`, or posts. It cannot be declared ready from here.")
        # Session first here too, and for the same reason in the opposite
        # direction. Writing health first meant a failed session write left the
        # peer recorded DOWN while guard-write still returned 0 — recorded
        # paused, mechanically writable, which is invariant 8 inverted. Now a
        # failed pause write applies nothing, and a failed health write leaves
        # the pause in force with a stale line: writes stay denied, and rerunning
        # the same command fixes it.
        if a.status=="rate_limited": set_pause(s,a.role,{"role":a.role,"reason":a.reason or "rate_limit","resume_at":resume,"at":now()})
        s["updated_at"]=now(); mb.save(s)
        mb.set_health(a.role,a.status,a.reason,resume,s=s)
    print(f"HEALTH role={a.role} status={a.status}"+(f" resume_at={resume}" if resume else "")); return 0

def append_fact(p,rec):
    """Append one delivery-fact record the way journals are appended: seal a
    torn tail first, escape raw line breaks, fsync. The caller holds the lock."""
    p.parent.mkdir(parents=True,exist_ok=True); seal_torn_tail(p)
    with p.open("a",encoding="utf-8",newline="\n") as f: f.write(journal_record(rec)); f.flush(); os.fsync(f.fileno())

def addressed_to(s,m,role):
    """Whether `role` is a recipient of `m`: the peer in a historic pair, or a
    named or broadcast recipient in a directed session."""
    if m.get("role")==role: return False
    if "to" not in m: return True
    return role in (m.get("to") or []) or bool(m.get("broadcast"))

def refuse(code,why):
    print(f"REFUSED {code}: {why}",file=sys.stderr); return 5

ENDPOINT_TOKEN_ENV="PAIR_ENDPOINT_TOKEN"

def endpoint_live(ep):
    """Registered, not retired, and its lease (if any) not yet over."""
    exp=ep.get("expires_at") if ep else None
    return bool(ep and ep.get("active")) and not (exp and parse_ts(exp)<=time.time())

def endpoint_token_ok(ep):
    """Whether the caller holds the token minted when `ep` was registered.

    Only its hash is on disk, so a participant that can read the mailbox still
    cannot act as another's endpoint by accident or by a model's stray command.
    It is not a defence against a hostile process of the same OS user, which
    the threat model trusts. The token travels in the environment, never argv,
    which the process list shows."""
    tok=os.environ.get(ENDPOINT_TOKEN_ENV,"")
    if not tok: return "no_token"
    if not hmac.compare_digest(hashlib.sha256(tok.encode()).hexdigest(),str(ep.get("token_sha256",""))): return "bad_token"
    return None

def endpoint(a):
    """Register or retire this participant's delivery endpoint.

    Adapter plumbing: the host endpoints and wake adapters call it, not a
    person. Generations only ever rise for the life of the session, so a
    stale endpoint's acceptance can always be told apart; retiring keeps the
    record as a tombstone for exactly that reason."""
    mb=MB(root(a.repo)); s=require(mb,a.role,allow_paused=True)
    p=mb.endpoint(s,a.role)
    with Lock(mb.role_lock(s,a.role)):
        old=readj(p) or {}
        if a.unregister:
            if not old.get("active"): return refuse("no_active_endpoint",f"{a.role} has no active endpoint to unregister")
            # An expired lease is over; retiring it needs no token, which is how a
            # restarted adapter that lost its token cleans up.
            bad=endpoint_token_ok(old) if endpoint_live(old) else None
            if bad: return refuse(bad,f"unregistering {a.role}'s endpoint needs the token it was registered with, in {ENDPOINT_TOKEN_ENV}")
            atomic(p,{**old,"active":False,"unregistered_at":now()})
            print(f"ENDPOINT {a.role} unregistered generation={old.get('generation')}"); return 0
        if not (a.transport and a.address): raise SystemExit("--register needs --transport and --address")
        # Replacing a live endpoint is rotation, and only its holder may rotate
        # it. Registering where none is active stays open: the mailbox has no
        # identity to check, so a receipt is a claim until the host binds the
        # endpoint to its process (spec D-8).
        # An expired lease is replaceable without its token: an adapter that died
        # with its token in memory must be able to come back in this session.
        # The new registration still takes a higher generation, so nothing the
        # old one accepted can be confused with the new one's.
        if endpoint_live(old):
            bad=endpoint_token_ok(old)
            if bad: return refuse(bad,f"{a.role} already has a live endpoint; replacing it needs its token in {ENDPOINT_TOKEN_ENV}")
        gen=int(old.get("generation",0))+1
        exp=fmt_ts(time.time()+a.ttl) if a.ttl else None
        token=secrets.token_hex(32)
        atomic(p,{"participant":a.role,"transport":a.transport,"address":a.address,"session_id":s["session_id"],
                  "generation":gen,"active":True,"registered_at":now(),"expires_at":exp,
                  "token_sha256":hashlib.sha256(token.encode()).hexdigest()})
    # The token is shown once, to the adapter that registered; it is the only
    # thing that can later accept for this endpoint.
    print(f"ENDPOINT {a.role} generation={gen}\nENDPOINT_TOKEN={token}"); return 0

def accept(a):
    """The endpoint's durable acceptance of one message: the only writer of a
    receipt. Validate, record with fsync, and only then answer, so a success
    without a durable receipt cannot happen. Idempotent per (msg_id, generation)."""
    mb=MB(root(a.repo)); s=require(mb,a.role,allow_paused=True)
    with Lock(mb.role_lock(s,a.role)):
        ep=readj(mb.endpoint(s,a.role)) or {}
        if not ep.get("active"): return refuse("no_active_endpoint",f"{a.role} has no active endpoint")
        if not endpoint_live(ep): return refuse("expired_endpoint",f"{a.role}'s endpoint lease expired at {ep.get('expires_at')}; fall back to watch")
        if int(ep.get("generation",0))!=a.generation:
            return refuse("stale_generation",f"generation {a.generation} is not {a.role}'s current {ep.get('generation')}")
        bad=endpoint_token_ok(ep)
        if bad: return refuse(bad,f"only {a.role}'s registered endpoint may accept; it presents its token in {ENDPOINT_TOKEN_ENV}")
        m=find_msg(mb,s,a.msg_id)
        if not m: return refuse("unknown_message",f"no message {a.msg_id} in this session")
        if not addressed_to(s,m,a.role): return refuse("not_addressed",f"{a.msg_id} is not addressed to {a.role}")
        rp=mb.receipts(s,a.role)
        seen=any(r.get("msg_id")==a.msg_id and r.get("generation")==a.generation for r in (journal_records(rp) if rp.exists() else []))
        if not seen: append_fact(rp,{"msg_id":a.msg_id,"generation":a.generation,"at":now(),"fact":"accepted"})
    print(f"ACCEPTED msg_id={a.msg_id} generation={a.generation}"); return 0

def record_push(a):
    """The sender's log of one push attempt. `sent` means the endpoint answered
    success; it never means delivered or read."""
    mb=MB(root(a.repo)); s=require(mb,a.role,allow_paused=True)
    if not a.msg_id.startswith(a.role+":"): return refuse("not_own_message",f"{a.msg_id} was not written by {a.role}")
    m=find_msg(mb,s,a.msg_id)
    if not m: return refuse("unknown_message",f"no message {a.msg_id} in this session")
    if a.to not in participants(s) or not addressed_to(s,m,a.to):
        return refuse("not_addressed",f"{a.msg_id} is not addressed to {a.to}")
    with Lock(mb.role_lock(s,a.role)):
        append_fact(mb.pushes(s,a.role),{"msg_id":a.msg_id,"to":a.to,"generation":a.generation,"at":now(),"outcome":a.outcome})
    print(f"PUSH_RECORDED msg_id={a.msg_id} to={a.to} outcome={a.outcome}"); return 0

def endpoint_lines(mb,s):
    out=[]
    for r in participants(s):
        ep=readj(mb.endpoint(s,r))
        if not ep: continue
        if not ep.get("active"): out.append(f"ENDPOINT {r} unregistered generation={ep.get('generation')}"); continue
        exp=ep.get("expires_at"); gone=bool(exp) and parse_ts(exp)<=time.time()
        out.append(f"ENDPOINT {r} transport={ep.get('transport')} generation={ep.get('generation')} expires={exp or '-'}"+(" EXPIRED" if gone else ""))
    return out

def unit_label(work_unit,unit_id):
    """`slug#ordinal`, or just the slug for a session predating `unit_id`.

    Sessions created before the field existed have none, and printing `#?` for
    them is noise about something that was never missing. Surfaced by this
    skill's own pairing, which had been running since before the change."""
    if not work_unit: return ""
    o=(unit_id or "").split("-")[0]
    return f"{work_unit}#{o}" if o else work_unit

def status(a):
    mb=MB(root(a.repo)); s=mb.active()
    if not s or s.get("status")=="parked":
        print("NO_ACTIVE_SESSION"); _print_parked(mb)
        if hasattr(a,"anchor_source"): print(anchor_line(a))
        return 1
    done=len(s.get("units") or [])
    unit=unit_label(s.get("work_unit"),s.get("unit_id")) + (f" ({done} closed)" if done else "")
    print(f"SESSION {s['session_id']} status={s['status']} phase={s['phase']} driver={s['driver']} owner={s['owner']} work={unit}")
    if (s.get("vcs") or mailbox_vcs(root(a.repo)))=="none": print(f"VCS none base={s.get('base_head')}")
    for c in (s.get("companions") or []): print(companion_line(c))
    for c in (s.get("absent_companions") or []): print(absent_line(c))
    for u in (s.get("units") or []): print(f"CLOSED_UNIT {unit_label(u.get('work_unit'),u.get('unit_id'))} closed_at={u.get('closed_at')} head={u.get('closed_head')}")
    # Only where it can differ from the historic rule: with one peer, that
    # peer is always the required reviewer and the line would be noise.
    if len(participants(s))>2:
        print(authority_line(s["owner"],authority(s)))
        for r,pz in sorted(pauses(s).items()): print(pause_line(s,r,pz))
    for r in participants(s):
        h=readj(mb.health(s,r),{}) or {}; print(f"{r}: {h.get('status','unknown')} updated={h.get('updated_at','unknown')} resume_at={h.get('resume_at') or '-'}")
    # Damage is reported, never skipped in silence: readers ignore an invalid
    # journal line, so this is the one place it becomes visible.
    for r in participants(s):
        jp=mb.journal(s,r)
        for n in (journal_invalid(jp) if jp.exists() else []): print(f"JOURNAL_INVALID {r} line={n}")
    for line in endpoint_lines(mb,s): print(line)
    caps=",".join(f"{r}:{'+'.join(v)}" for r,v in sorted((s.get("caps") or {}).items())) or "-"
    if schema(s)==1:
        un=" ".join(f"{r}=#{x[0]}..#{x[1]}" for r in participants(s) for x in [unacked_range(mb,s,r)] if x)
    else:
        un=" ".join(f"{r}=" + ",".join(f"{snd}:{a}..{snd}:{b}" for snd,a,b in u)
                    for r in participants(s) for u in [unacked_v2(mb,s,r)] if u)
    print(f"DELIVERY mode={delivery(s)} caps={caps}"+(f" unacked {un}" if un else ""))
    if schema(s)==2 and len(participants(s))>2:
        for mid,e in sorted(waiting_map(s).items()):
            print(f"WAITING {mid} {e.get('kind')} from={e.get('from')} pending={','.join(e.get('pending') or []) or '-'} "
                  f"answered={','.join(e.get('answered_by') or []) or '-'}")
    _print_parked(mb)
    if hasattr(a,"anchor_source"): print(anchor_line(a))
    return 0

def _print_parked(mb):
    parked=parked_sessions(mb)
    for k,v in parked.items(): print(f"PARKED {k} work={v['work_unit']} from={v.get('parked_from')} parked_at={v.get('parked_at')}")

def guard(a):
    """May this role write, and — with `--path` — may it write *there*?

    Without `--path` this answers only about the session's own working tree, as
    it always has. With one it also covers a declared companion, so the sibling
    write is governed by a mechanism rather than by a promise. Every denial is
    exit 4; the stderr line says which of the three reasons applied."""
    repo=root(a.repo); mb=MB(repo); s=require(mb,a.role,allow_paused=True)
    if s.get("status")!="active" or s.get("owner")!=a.role or s.get("handoff"):
        print(f"WRITE_DENIED status={s.get('status')} owner={s.get('owner')} handoff={'yes' if s.get('handoff') else 'no'}",file=sys.stderr); return 4
    if not a.path: return 0
    q=Path(a.path)
    q=(q if q.is_absolute() else (Path.cwd()/q)).resolve()
    if q==repo or repo in q.parents: return 0
    c,rel=companion_of(s,q)
    if not c:
        print(f"WRITE_DENIED reason=outside-session-scope path={q}\n"
              f"  the session owns {repo}; a sibling repository is evidence, not workspace,\n"
              f"  unless it is declared in {COMPANION_FILE} and the session was started after that",file=sys.stderr); return 4
    if not glob_match(c["write"],rel):
        print(f"WRITE_DENIED reason=outside-companion-scope companion={c['name']} path={rel}\n"
              f"  declared write scope: {', '.join(c['write'])}",file=sys.stderr); return 4
    return 0

def resolve_head(repo,mode=None):
    """The current commit, or "UNBORN" when there are no commits yet.

    `git rev-parse HEAD` on an empty repository prints the literal string "HEAD"
    to stdout and reports the failure only on stderr, so a plain `or "UNBORN"`
    fallback never fired and every consumer got "HEAD" as if it were a commit.
    `--verify` makes the failure show up where it can be seen.

    In no-VCS mode the same slot carries a `T:`-prefixed content digest of the
    whole tree. The prefix is not decoration: it keeps a tree digest from ever
    being read as, or compared against, an abbreviated commit id."""
    if (mode or mailbox_vcs(repo))=="none": return tree_digest(scan_tree(repo))
    return git(repo,"rev-parse","--verify","--quiet","HEAD",check=False).strip() or "UNBORN"

def snap_rows(repo,mode=None):
    """One scan, used for both the digest and the manifest written beside it.

    `snap` and `write_manifest` each used to scan, so a file that landed between
    the two calls left `base_head` describing one state and the stored base
    manifest another. Nothing detects that afterwards: `changed_paths` compares
    against the manifest while every boundary check compares the digest, so work
    that arrived during the boundary is silently absent from the review set. The
    rows are the single source both are derived from.

    Returns `(snapshot, rows)`, with `rows` None in Git mode."""
    if (mode or mailbox_vcs(repo))=="none":
        rows=scan_tree(repo)
        return {"head":tree_digest(rows),"status":[]},rows
    return snap(repo,"git"),None

def snap(repo,mode=None):
    """The state a handoff, a park, or a unit boundary is pinned to.

    No-VCS mode leaves `status` empty rather than inventing entries for it: Git's
    porcelain lines describe a tree against an index and a commit, and neither
    exists here. Nothing is lost, because the head digest is already content-
    derived over every file — what Git splits across a commit id and a dirty
    list, this carries in one value."""
    if (mode or mailbox_vcs(repo))=="none": return {"head":tree_digest(scan_tree(repo)),"status":[]}
    return {"head":resolve_head(repo,"git"),"status":git(repo,"status","--porcelain=v1","--untracked-files=all",check=False).splitlines()}
def handoff_offer(a):
    repo=root(a.repo); mb=MB(repo)
    with Lock(mb.lock):
        s=require(mb,a.role)
        if s["owner"]!=a.role: raise SystemExit("only owner may offer handoff")
        others=[r for r in participants(s) if r!=a.role]
        to=getattr(a,"to",None)
        if to is None:
            if len(others)!=1: raise SystemExit(f"handoff-offer needs --to <participant> with {len(participants(s))} participants")
            to=others[0]
        elif to not in others:
            raise SystemExit(f"--to {to} is not another participant (participants: {', '.join(participants(s))})")
        handoff_authority(s,to)          # refused here, before anything is pinned
        # Companions are pinned alongside the anchor tree: the incoming owner
        # accepts a state, and a plan record that moved between offer and accept
        # is as much a changed state as a moved HEAD.
        q=snap(repo); cq=[{"name":c["name"],**snap(Path(c["root"]),c.get("vcs"))} for c in (s.get("companions") or [])]
        ep=int(s["ownership_epoch"])+1; s["handoff"]={"epoch":ep,"from":a.role,"to":to,**q,"companions":cq,"offered_at":now()}; s["updated_at"]=now(); mb.save(s)
    multi=len(others)>1
    mb.post(a.role,"HANDOFF_OFFER",f"epoch={ep} to={to} head={q['head']} dirty={len(q['status'])}",True,
            to=to if multi else None,internal=True); print(f"HANDOFF_OFFERED epoch={ep} to={to}"); return 0
def handoff_accept(a):
    repo=root(a.repo); mb=MB(repo)
    with Lock(mb.lock):
        s=require(mb,a.role); o=s.get("handoff")
        if not o or o.get("to")!=a.role: raise SystemExit("no handoff offered to this role")
        q=snap(repo)
        if q["head"]!=o["head"] or q["status"]!=o["status"]: raise SystemExit("working tree changed since handoff offer; user must resolve ownership")
        for c in (s.get("companions") or []):
            was=next((x for x in (o.get("companions") or []) if x.get("name")==c["name"]),None)
            # A companion absent from the offer means the offer predates it.
            # Refuse rather than accept blind: the two roles would otherwise
            # disagree about what was handed over.
            if not was: raise SystemExit(f"companion {c['name']} was not pinned by this handoff offer; re-offer before accepting")
            live=snap(Path(c["root"]),c.get("vcs"))
            if live["head"]!=was["head"] or live["status"]!=was["status"]:
                raise SystemExit(f"companion {c['name']} changed since handoff offer; user must resolve ownership")
        # The owner and the reviewer sets change in this one write. A
        # reviewer added here starts with a floor at its current sequence, so
        # nothing it wrote as the owner can count as its verdict.
        old=s["owner"]
        if schema(s)==2:
            s["authority"]=handoff_authority(s,a.role)
            if old in s["authority"]["required_reviewers"]:
                fl=dict(s.get("verdict_floor") or {}); fl[old]=int((readj(mb.latest(s,old),{}) or {}).get("seq",0)); s["verdict_floor"]=fl
        s["owner"]=a.role; s["ownership_epoch"]=o["epoch"]; s["handoff"]=None; s["waiting"]=None; settle_status(s); s["updated_at"]=now(); mb.save(s)
    multi=len(participants(s))>2
    mb.post(a.role,"HANDOFF_ACCEPT",f"epoch={o['epoch']} head={o['head']}",broadcast=multi,internal=True); print(f"HANDOFF_ACCEPTED epoch={o['epoch']} owner={a.role}"); return 0

def phase(a):
    mb=MB(root(a.repo))
    with Lock(mb.lock):
        s=require(mb,a.role); s["phase"]=a.phase; s["updated_at"]=now(); mb.save(s)
    return 0

# Kinds that take a position the owner must not close over, plus anything still
# awaiting an explicit reply. Selected positively and grounded in meanings the
# protocol already defines, rather than an invented skip-list: SKILL.md calls
# STOP blocking and NOTE/STEER explicitly non-blocking, ESCALATE is unresolved
# disagreement, CHALLENGE is a design objection, and an outstanding expect_reply
# is an open question. Letting a NOTE veto a close would contradict the same
# document that tells a navigator to send one.
DECISION_KINDS={"VERDICT","STOP","ESCALATE","CHALLENGE"}

def latest_decision(mb,s,role):
    """The peer's most recent message that takes a position on the work."""
    p=mb.journal(s,role); last={}
    if not p.exists(): return last
    for m in journal_records(p):
        if m.get("kind") in DECISION_KINDS or m.get("expect_reply"): last=m
    return last

def changed_paths(repo,base,mode=None,manifest=None):
    """Everything committed since `base`, plus everything currently uncommitted.

    One definition, used for the session's own tree and for every companion, so
    a reviewer cannot be shown two different notions of "changed".

    No-VCS mode answers the same question from the manifest written when the unit
    opened: every path whose content row differs, plus every path added or
    removed. Nothing goes in the "committed" half, because there are no commits —
    which is exactly the limitation the review request has to state out loud. A
    missing manifest returns the whole tree rather than nothing: over-reporting
    wastes a reviewer's attention, under-reporting hides a change.

    -z throughout: NUL-separated, never quoted, so a path containing a space, a
    tab, or a non-ASCII byte survives whatever core.quotePath is set to. The
    textual forms mangle all three, and a fixed-prefix slice additionally turned
    a rename's "old -> new" into one nonexistent path."""
    if (mode or mailbox_vcs(repo))=="none":
        live={r:(k,str(s),d) for r,k,s,d in scan_tree(repo)}
        was=read_manifest(manifest) if manifest else None
        if was is None: return [],sorted(live)
        return [],sorted({p for p in set(live)|set(was) if live.get(p)!=was.get(p)})
    if base=="UNBORN":
        # The unit began before the first commit, so "committed since the base"
        # is the whole tree. `git diff HEAD` would compare HEAD to the worktree
        # instead and hand back nothing once the work was committed and the tree
        # was clean - an empty review set for a finished change.
        tracked=[x for x in git(repo,"ls-tree","-r","--name-only","-z","HEAD",check=False).split(chr(0)) if x]
    else:
        tracked=[x for x in git(repo,"diff","--name-only","-z",f"{base}..HEAD",check=False).split(chr(0)) if x]
    parts=[x for x in git(repo,"status","-z","--porcelain=v1","--untracked-files=all",check=False).split(chr(0)) if x]
    dirty=[]; i=0
    while i<len(parts):
        ent=parts[i]; code,path=ent[:2],ent[3:]
        dirty.append(path)
        # R/C can appear in either column: a staged rename puts it in the index
        # slot, an unstaged one in the worktree slot. Checking only the first
        # dropped the source path of a worktree rename.
        if len(code)>1 and ("R" in code or "C" in code) and i+1<len(parts):
            dirty.append(parts[i+1]); i+=1
        i+=1
    return tracked,dirty

def digest_rows(root_dir,paths):
    rows=[]
    for f in paths:
        fp=Path(root_dir)/f
        try: rows.append((hashlib.sha256(fp.read_bytes()).hexdigest()[:12],f))
        except (FileNotFoundError,IsADirectoryError,PermissionError): rows.append(("gone/unreadable",f))
    return rows

def verify_request(a):
    """Emit a self-contained verification request for a fresh, uninvolved reader.

    By the end of a long session both roles share one story about the work, and
    a review by someone who already believes it is worth less than it looks.
    This produces a request a cold reader can act on: what to check, how, and
    against which exact bytes.

    What the isolation actually is: nothing here writes, the block carries no
    mailbox path and no transcript excerpt, and the harness stays two-role -
    no third journal, health file, or cursor, because the reply comes back
    through the human who ran it. What it is *not* is a sandbox. The block names
    the working-tree root, and a reader who knows this skill's conventions could
    walk to the mailbox from there. The guarantee is that nothing hands the
    narrative over, plus an explicit prohibition against going to look; it is not
    that looking is impossible.

    Its verdict is advisory. `complete` requires the *peer's* AGREE and reads the
    peer's journal to find it, so a verdict pasted back can inform the navigator
    but can never stand in for it. Anything else would weaken the one mechanical
    guarantee that a second agent signed off."""
    repo=root(a.repo); mb=MB(repo); s=require(mb,a.role)
    vcs=s.get("vcs") or mailbox_vcs(repo)
    files=list(a.file or [])
    base=s.get("base_head") or "UNBORN"
    if not files:
        tracked,dirty=changed_paths(repo,base,vcs,mb.manifest(s))
        # The default set sweeps untracked files too, because a brand-new source
        # file is exactly what a reviewer must see. That also sweeps the scratch
        # this very command is being fed and piped into, which is noise at best
        # and a hash stale before it is read at worst. Drop what we can identify:
        # our own criteria file, and the mailbox. The rest is why `--file` exists.
        skip=set()
        if a.criteria_file:
            try: skip.add(Path(a.criteria_file).resolve().relative_to(repo).as_posix())
            except ValueError: pass
        # Component match, not prefix: `.pair-programming-notes.md` is an
        # ordinary file and startswith() was hiding it from review.
        def in_mailbox(f): return Path(f).parts[:1]==(PAIR_DIR,)
        files=sorted({f for f in tracked+dirty if f and not in_mailbox(f) and f not in skip})
    rows=digest_rows(repo,files)
    crit=Path(a.criteria_file).read_text(encoding="utf-8").strip() if a.criteria_file else (a.criteria or "").strip()
    if not crit: raise SystemExit("acceptance criteria required; a request a cold reader cannot act on is not worth the round")
    try: rnd=int(a.round)
    except (TypeError,ValueError): raise SystemExit(f"--round must be a whole number 1-3, not {a.round!r}")
    if not 1<=rnd<=3: raise SystemExit(f"--round must be 1-3 (the review cap); got {rnd}")
    out=["FRESH-EYES VERIFICATION REQUEST","",
         f"Working tree: {repo}",
         f"Work unit: {unit_label(s.get('work_unit'),s.get('unit_id'))}   Review round: {rnd}",
         *([f"Your findings inform the required reviewers ({', '.join(authority(s)['required_reviewers'])})"
            +(f" and the advisers ({', '.join(authority(s)['advisers'])})" if authority(s)['advisers'] else "")
            +"; they never replace a reviewer's own verdict."] if len(participants(s))>2 else []),
         (f"Base content digest for this unit: {base}" if vcs=="none" else f"Base commit for this unit: {base}"),""]
    out += (["This directory is not under version control. There is no diff to run and no",
             "history to walk: the change is defined as the difference between the file",
             "digests recorded when this unit opened and the bytes on disk now. Two",
             "consequences you should not have to infer —",
             "  - an edit that was made and then reverted is invisible here, and so is every",
             "    intermediate state; you are reviewing the end state only.",
             "  - a path listed below whose digest reads gone/unreadable was deleted.",
             "Read the named files directly."] if vcs=="none" else
            ["Reproduce the change with either of these (both non-mutating):",
             (f"  git diff {base}..HEAD" if base!="UNBORN" else "  git log --patch   # the unit began before the first commit"),
             "  git status --porcelain=v1 --untracked-files=all   # uncommitted work, if any"])
    out += ["",
         "You were opened in a new session so that you carry none of the implementation",
         "narrative. That absence is the whole value — do not reconstruct it, and do not go",
         "looking for it. Read only the repositories and the change described below.","",
         "Rules, all hard:",
         "- Read-only end to end. Create, edit, or delete nothing — no commits, no branches,",
         "  no staging, not even an untracked or generated file. If a check below would write,",
         "  report it unrun and substitute a non-writing equivalent if one exists.",
         "- Do not look for, or read, any pairing mailbox, journal, transcript, or session",
         "  log, even if you find one. It would anchor you to the story this role exists to",
         "  be free of.",
         "- If what you find does not match what is described here, say so and stop rather",
         "  than improvising a scope.","",
         "Acceptance criteria:",crit,"",
         "Files under review (sha256 prefix — re-hash before judging; a mismatch means the",
         "tree moved under this request and you should say so rather than review stale bytes):"]
    out += [f"  {h}  {f}" for h,f in rows] or ["  (none — the request names no changed file, which is itself worth reporting)"]
    if not a.file:
        out += ["","(That set is: every path whose content differs from the manifest taken when",
                "this unit opened, plus every path added or removed since. Scratch files count,",
                "and so does anything a build wrote. If something there is not work, the request",
                "should have pinned the set explicitly; say so rather than reviewing it.)"] if vcs=="none" else \
               ["","(That set is: everything committed since this unit's base, plus everything",
                "currently uncommitted — including files that were already dirty when the unit",
                "began, and including untracked scratch. It is deliberately over-broad rather",
                "than a true delta. If something there is not work, the request should have",
                "pinned the set explicitly; say so rather than reviewing it.)"]
    # Companions are part of the change, so they are part of the request. A plan
    # record is exactly where a claim outlives the code that justified it, which
    # makes it the last place an omission is safe.
    # Read from the frozen record, never re-resolved: a binding edited mid-session
    # must not change what this request says the boundary was.
    for c in (s.get("absent_companions") or []):
        out += ["",f"Companion repository: {c['name']}, ABSENT on this machine ({c['reason']})",
                f"  Declared for: {c['why']}",
                f"  Declared write scope, not granted this session: {', '.join(c['write'])}"]
    for ci,c in enumerate(s.get("companions") or []):
        croot=Path(c["root"]); cbase=c.get("head") or "UNBORN"; cvcs=c.get("vcs") or "git"
        ct,cd=changed_paths(croot,cbase,cvcs,mb.manifest(s,ckey(c,ci)))
        allp=sorted({x for x in ct+cd if x}); inside=[f for f in allp if glob_match(c["write"],f)]
        outside=[f for f in allp if f not in set(inside)]
        out += ["",f"Companion repository: {c['name']}",
                f"  Root: {c['root']}",
                f"  Declared for: {c['why']}",
                f"  This session may write only: {', '.join(c['write'])}",
                (f"  Base content digest for this unit: {cbase}" if cvcs=="none" else f"  Base commit for this unit: {cbase}"),
                ("  Not under version control: the set below is a digest delta against that base,"
                 " with no history behind it" if cvcs=="none" else
                 f'  Reproduce with: git -C "{c["root"]}" diff {cbase}..HEAD' if cbase!="UNBORN"
                 else f'  Reproduce with: git -C "{c["root"]}" log --patch'),
                "  Files under review there (same digest recipe):"]
        out += [f"    {h}  {f}" for h,f in digest_rows(croot,inside)] or ["    (none changed)"]
        if outside:
            out += ["","  Changed there but OUTSIDE that declared scope. This session was not"
                       " permitted",
                    "  to write these, so treat them as a finding rather than as work under"
                       " review:"]
            out += [f"    {f}" for f in outside]
    checks=list(a.check or [])
    out += ["","Checks to run (non-mutating):"] + ([f"  {c}" for c in checks] or
            ["  (none named — inspect the diff and say what you could not verify without one)"])
    out += ["","Answer with a verdict whose first line is exactly one of:",
            "  AGREE round=<n> blocking=0 important=<n>",
            "  REVISE round=<n> blocking=<n> important=<n>",
            f"using round={rnd}. Then list only actionable findings, one per line, as",
            "  path:line — problem — fix",
            "Name the sha prefixes you formed the verdict against. Then stop.","",
            "This verdict is advisory. It goes back to the pair, who weigh it and issue their",
            "own review. Nothing you write here closes the work, so say what you actually",
            "found rather than what would let it pass."]
    print("\n".join(out)); return 0

def journal_rows(mb,s,role):
    p=mb.journal(s,role); out=[]
    if not p.exists(): return out
    return journal_records(p)

def thread_of(m): return m.get("thread_id") or m.get("msg_id")

# A STOP is a correctness or safety objection and an ESCALATE is unresolved
# disagreement: neither may be scoped away by which thread it was posted in.
GLOBAL_DECISIONS={"STOP","ESCALATE"}

def reviewer_standing(mb,s,r):
    """Required reviewer r's standing decision on the current unit, or why none.

    Returns (message, None) or (None, reason). With more than one non-owner a
    reviewer's position is read from the review it was asked for, not from
    whatever it last said to anyone:

    - the candidates are r's messages in this unit above r's verdict floor that
      are either a VERDICT, CHALLENGE or open question to the owner in the
      thread of the owner's latest REVIEW_REQUEST naming r, or any STOP or
      ESCALATE by r, in any thread;
    - the latest candidate stands. Only a later AGREE by r in the review
      thread retires r's own STOP or ESCALATE; nobody can clear it for r."""
    o=s["owner"]; unit=s.get("unit_id"); floor=int((s.get("verdict_floor") or {}).get(r,0))
    reqs=[m for m in journal_rows(mb,s,o) if m.get("kind")=="REVIEW_REQUEST" and m.get("unit_id")==unit and r in (m.get("to") or [])]
    thread=thread_of(reqs[-1]) if reqs else None
    best=None
    for m in journal_rows(mb,s,r):
        if m.get("unit_id")!=unit or int(m.get("seq",0))<=floor: continue
        # Nothing r wrote as the owner is r's verdict on the work. Messages
        # without the stamp predate it and are judged by the floor alone.
        if m.get("owner")==r: continue
        k=m.get("kind")
        scoped=(thread is not None and thread_of(m)==thread and o in (m.get("to") or [])
                and (k in ("VERDICT","CHALLENGE") or m.get("expect_reply")))
        if scoped or k in GLOBAL_DECISIONS:
            if best is None or int(m.get("seq",0))>int(best.get("seq",0)): best=m
    if best is None:
        return None,(f"{r}: no review requested from it in this unit" if thread is None else f"{r}: no verdict since the unit boundary or resume")
    first=(best.get("body") or "").splitlines()[0] if best.get("body") else ""
    if best.get("kind")!="VERDICT" or not first.startswith("AGREE "):
        return None,f"{r}: {best.get('kind')} #{best.get('seq')} stands"
    return best,None

def peer_agreed(mb,s,role):
    """The peer's standing AGREE, or a SystemExit explaining why there isn't one.

    Shared by `complete` and `next-unit` because both close a unit of work and
    neither may do it on the owner's say-so alone. A second copy of this check
    would be a second definition of what counts as sign-off."""
    if len([x for x in participants(s) if x!=role])>1:
        # Every required reviewer; a majority never closes. Advisers are never read.
        blocks=[why for r in authority(s)["required_reviewers"] for _,why in [reviewer_standing(mb,s,r)] if why]
        if blocks: raise SystemExit("not every required reviewer agrees: "+"; ".join(blocks))
        return None
    peer=latest_decision(mb,s,the_peer(s,role)); first=(peer.get("body") or "").splitlines()[0] if peer.get("body") else ""
    if peer.get("kind")!="VERDICT" or not first.startswith("AGREE "): raise SystemExit("peer's latest decision is not an AGREE verdict")
    # Sequence alone is not enough. `post` reads the session to stamp a message,
    # then appends outside the mailbox lock, so a verdict composed against the
    # previous work unit can land AFTER the boundary sampled verdict_floor:
    # higher sequence, wrong unit, and it would authorize a close on work it was
    # never about. Every message already carries the unit it was written under;
    # that stamp is the discriminator, and sequence only orders within a unit.
    if peer.get("unit_id")!=s.get("unit_id"):
        raise SystemExit(f"peer's AGREE was given on work unit {peer.get('work_unit')!r} ({peer.get('unit_id')}), not the current {s.get('work_unit')!r} ({s.get('unit_id')}); a fresh verdict is required")
    floor=int((s.get("verdict_floor") or {}).get(the_peer(s,role),0))
    if int(peer.get("seq",0))<=floor: raise SystemExit("peer's AGREE predates the last unit boundary or resume; the review was invalidated and a new verdict is required")
    return peer

def next_unit(a):
    """Close the current work unit and open the next one, atomically.

    A session used to hold exactly one task, so a second piece of work meant
    complete + start: a new mailbox, a new journal, and the pairing's continuity
    thrown away for nothing. This is that boundary without the teardown.

    It is deliberately one operation rather than a close followed by an open.
    Two steps leave a window in which the session is active with no current unit
    and an undefined phase — a state nothing else in the harness knows how to
    read, reachable by any crash between them.

    Preconditions are part of the contract, not defensive coding. Only the
    current owner may cross the boundary, and not while a handoff is pending:
    `--role` is a protocol claim rather than authentication (invariant 4), so
    membership alone would let the navigator retire the driver's work unit out
    from under it. And the peer's AGREE is required for the same reason
    `complete` requires it — the unit is being closed either way."""
    repo=root(a.repo); mb=MB(repo); task=body(a).strip()
    if not task: raise SystemExit("the next work unit needs a task")
    with Lock(mb.lock):
        s=require(mb,a.role)
        if s["owner"]!=a.role: raise SystemExit("only the current owner may open the next work unit")
        if s.get("handoff"): raise SystemExit("a handoff is pending; settle ownership before opening the next work unit")
        # Validated before anything changes: a refused declaration leaves the
        # unit open exactly as it was. Without --advisers the new unit keeps the
        # current advisers, re-checked against the current owner.
        adv=getattr(a,"advisers",None)
        if adv is None: adv=",".join(authority(s)["advisers"])
        auth=declare_authority(participants(s),s["owner"],adv)
        peer_agreed(mb,s,a.role)
        done=s.get("units") or []
        done.append({"work_unit":s["work_unit"],"unit_id":s.get("unit_id"),
                     "task":s["task"],"opened_at":s.get("unit_opened_at") or s.get("created_at"),"closed_at":now(),
                     "base_head":s.get("base_head"),"base_status":s.get("base_status"),
                     "companions":s.get("companions"),
                     "verdict_floor":s.get("verdict_floor"),"closed_head":resolve_head(repo),
                     **({"owner":s["owner"],"authority":{k:authority(s)[k] for k in ("required_reviewers","advisers")}} if schema(s)==2 else {})})
        s["units"]=done
        s["task"]=task; s["work_unit"]=a.work_unit or slug(task); s["unit_opened_at"]=now()
        s["unit_id"]=f"{len(done)+1}-"+uuid.uuid4().hex[:8]
        # One scan per tree here too — the boundary must not be described by one
        # scan and reviewed against another.
        q,rows=snap_rows(repo); crows={}
        s["base_head"]=q["head"]; s["base_status"]=q["status"]
        # Re-base the companions on the same boundary. The declaration itself is
        # NOT reloaded: it was frozen at `start`, and a boundary that quietly
        # picked up an edited config would widen the write scope at the very
        # moment review standing is being reset.
        # `.get`, not `[k]`: "vcs" is absent from every companion frozen before
        # no-VCS mode existed, and `snap` reads a missing one as Git — which is
        # what those sessions are.
        s["companions"]=snap_companions([{k:c.get(k) for k in ("name","spec","root","vcs","mkey","write","why")} for c in (s.get("companions") or [])],crows)
        # Re-base the manifests too, under the NEW unit id, from those same rows.
        # The finished unit keeps its own, so a later audit of it still has
        # something to diff against.
        if rows is not None: write_manifest(mb.manifest(s),rows)
        for i,r in crows.items(): write_manifest(mb.manifest(s,ckey(s["companions"][i],i)),r)
        # The boundary invalidates review standing exactly as `resume` does, and
        # by the same mechanism: an AGREE earned on the finished unit says nothing
        # about the one starting now. Sequence, not timestamp — sequence is
        # per-role monotonic and two messages can share a wall-clock second.
        s["verdict_floor"]={r:int((readj(mb.latest(s,r),{}) or {}).get("seq",0)) for r in participants(s)}
        if schema(s)==2: s["authority"]=auth
        s["phase"]="huddle"; s["waiting"]=None; settle_status(s); s["updated_at"]=now(); mb.save(s)
    print(f"NEXT_UNIT session={s['session_id']} work={s['work_unit']} closed={done[-1]['work_unit']} units={len(done)+1}")
    print("REVIEW_INVALIDATED phase=huddle; a new peer AGREE is required before the next boundary or complete")
    return 0

def complete(a):
    mb=MB(root(a.repo))
    # Validate and close under one lock, on one session object. Re-reading
    # mb.active() inside the lock — as this did — closes whatever is current
    # rather than what was checked, so a park/start/resume landing between the
    # two steps let an AGREE for session A close session B.
    with Lock(mb.lock):
        s=require(mb,a.role)
        if s["owner"]!=a.role: raise SystemExit("only current owner may close")
        peer_agreed(mb,s,a.role)
        s["status"]="completed"; s["phase"]="complete"; s["waiting"]=None; s["updated_at"]=now(); mb.save(s)
    print(f"COMPLETED session={s['session_id']}"); return 0

def scan_sessions(mb,strict=False):
    """Every session on disk, keyed by its **directory name**.

    One scan, two callers. Keyed by the directory and never by the `session_id`
    inside the JSON: that file is ordinary on-disk state, so a corrupt or hostile
    traversal-shaped id would otherwise pass a membership check and then be
    rebuilt into a path pointing somewhere else.

    `strict` is what the purge uses. A reader that only wants to list parked work
    can skip anything it cannot parse; a command that is about to delete and then
    decide "this mailbox is finished" cannot, because every entry it silently
    skipped is state that survives while the ignore rule protecting it is
    removed. In strict mode the unreadable, the unexpected, and the linked all
    raise before anything is touched."""
    base=mb.base/"sessions"; out={}
    if reparse(base):
        if strict: raise SystemExit(f"refusing to purge through a linked sessions directory: {base}")
        return out
    if not base.is_dir():
        if os.path.lexists(base):
            if strict: raise SystemExit(f"{base} exists but is not a directory; refusing to purge around it")
        return out
    for d in sorted(base.iterdir()):
        if reparse(d):
            if strict: raise SystemExit(f"refusing to purge around a linked entry: {d}")
            continue
        if not d.is_dir():
            if strict: raise SystemExit(f"unexpected file in the sessions directory: {d}")
            continue
        # A session that cannot be read is skipped by a tolerant caller and
        # refused by a strict one. Listing what can be resumed must not fail
        # because some unrelated directory holds malformed JSON; deleting must
        # not proceed past state it could not understand.
        why=None
        try:
            try: sf=session_file(d)
            except SystemExit as e:
                if strict: raise
                continue
            sj=json.loads(sf.read_text(encoding="utf-8"))
        except FileNotFoundError:
            sj=None; why="has no session.json"
        except (json.JSONDecodeError,UnicodeDecodeError,OSError) as e:
            sj=None; why=f"has an unreadable {sf.name} ({type(e).__name__})"
        if not isinstance(sj,dict):
            # Every non-object outcome gets a reason, including a valid `null`
            # or a list — those parse cleanly and so reach here with none set.
            if why is None: why=f"has a session.json holding {type(sj).__name__}, not an object"
            if strict: raise SystemExit(f"session directory {d.name!r} {why}; refusing to purge around it")
            continue
        if sj.get("session_id")!=d.name:
            # Tolerant means tolerant here as well: a listing should survive one
            # inconsistent neighbour. Deleting must not, because a stored id that
            # disagrees with its directory is exactly the state a rebuilt path
            # would resolve somewhere unintended.
            if strict: raise SystemExit(f"session directory {d.name!r} holds id {sj.get('session_id')!r}; refusing to act on mismatched mailbox state")
            continue
        out[d.name]=sj
    return out

def parked_sessions(mb):
    """The parked subset. Tolerant by design — listing what can be resumed should
    not fail because some unrelated entry is unreadable."""
    return {k:v for k,v in scan_sessions(mb).items() if v.get("status")=="parked"}

def reparse(p):
    """True for a symlink, junction, or mount point - and true when we cannot tell.

    A scoped delete that follows one of these stops being scoped, so this fails
    **closed**: a stat that errors returns True, refusing the operation, rather
    than reporting "not a link" about a path it could not read. Windows junctions
    are caught by the reparse-point attribute (present since 3.5) rather than
    `Path.is_junction`, which would impose a 3.12 floor for no gain."""
    try:
        if p.is_symlink(): return True
        st=p.lstat()
        if bool(getattr(st,"st_file_attributes",0) & 0x400): return True  # FILE_ATTRIBUTE_REPARSE_POINT
        try:
            if p.is_mount(): return True
        except (OSError,ValueError,AttributeError): return True
        return False
    except FileNotFoundError: return False
    except (OSError,ValueError): return True

def rmtree_no_links(p):
    """Delete a tree, refusing to descend through any reparse point.

    `reparse` is asked before `is_dir`, always: `is_dir()` on a link stats its
    *target*, which is the step the no-follow contract says never happens. And a
    link that appears between preflight and here is refused rather than unlinked
    — preflight said this tree was safe, so anything new in it is a surprise, and
    surprises during a delete are not resolved by deleting them."""
    if reparse(p): raise SystemExit(f"refusing to delete through a link: {p}")
    for child in sorted(p.iterdir()):
        if reparse(child): raise SystemExit(f"a link appeared under {p} after preflight: {child}; nothing further was deleted")
        if child.is_dir(): rmtree_no_links(child)
        else: child.unlink()
    p.rmdir()

def check_deletable(p):
    """Raise before anything is removed if this tree cannot be safely deleted."""
    if reparse(p): raise SystemExit(f"refusing to delete through a link: {p}")
    for child in sorted(p.iterdir()):
        if reparse(child): raise SystemExit(f"refusing to delete through a link: {child}")
        if child.is_dir(): check_deletable(child)

def sharing_worktrees(repo):
    """Other worktrees that resolve the same info/exclude as this one.

    A linked worktree reaches info/exclude through the common directory, so the
    file is shared state. Whether *this* tree still has a mailbox says nothing
    about the others', and removing a shared rule on that basis would un-ignore
    someone else's live one.

    No-VCS mode has no worktrees and no shared exclude file, so the answer is
    empty rather than unknown."""
    if mailbox_vcs(repo)=="none": return []
    mine=exclude_path(repo)
    out=[]
    blocks=git(repo,"worktree","list","--porcelain",check=False).splitlines()
    for line in blocks:
        if not line.startswith("worktree "): continue
        w=Path(line[len("worktree "):]).resolve()
        if w==repo: continue
        try:
            if exclude_path(w)==mine: out.append(w)
        except SystemExit: pass
    return out

def at_lines(idx):
    """"line 7" / "lines 7, 12" — so every reported candidate is locatable."""
    return "line "+str(idx[0]+1) if len(idx)==1 else "lines "+", ".join(str(i+1) for i in idx)

def purge(a):
    """Reclaim mailbox state without destroying anything still load-bearing.

    Nothing removed `.pair-programming/` before this, so every repository ever
    paired on kept its sessions and its ignore rule forever. The reason it is not
    a simple delete is that this mailbox is an archive, not one session's scratch:
    `abandon` deliberately keeps journals readable through `transcript`, and a
    parked session is resumable work. So the selector is explicit, the default is
    a dry run, and only terminal sessions are eligible.

    The ignore rule is only removed when this tool can prove it wrote it. A rule
    without our marker beside it may be the user's own, and silently un-ignoring
    a directory in someone else's repository is worse than leaving a stale line."""
    repo=root(a.repo); mb=MB(repo)
    if not (a.session or a.all_closed): raise SystemExit("pass --session <id> or --all-closed; purge does not guess a target")
    # Link question first, here too. Asking is_dir() first follows a mailbox
    # symlink once before refusing it, and reports a broken one as "no mailbox"
    # rather than as the thing to refuse.
    if reparse(mb.base): raise SystemExit(f"refusing to touch a linked mailbox: {mb.base}")
    if not mb.base.is_dir(): raise SystemExit("no mailbox here")
    with Lock(mb.lock):
        known=scan_sessions(mb,strict=True)
        if a.session:
            # Membership in the scanned map, never a path built from the argument,
            # so an unknown id and a traversal-shaped one fail the same way.
            if a.session not in known: raise SystemExit(f"unknown session {a.session!r}")
            targets={a.session:known[a.session]}
        else:
            targets={k:v for k,v in known.items() if v.get("status") in TERMINAL}
        # A session written under rules this build does not speak may not mean
        # "finished" by the same word; deleting it on a guess is not undoable.
        for k,v in targets.items(): check_protocol(v,f"session {k}")
        keep=[]
        for k,v in sorted(known.items()):
            st=v.get("status")
            if k in targets and st not in TERMINAL:
                raise SystemExit(f"session {k} is {st}; only completed or abandoned sessions may be purged"
                                 + (" — resume it and abandon it first" if st=="parked" else ""))
            if k not in targets: keep.append((k,st))
        if not targets: raise SystemExit("nothing to purge; no completed or abandoned session found")
        # Preflight the whole selected forest before touching anything. The walk
        # used to discover a problem partway through and leave earlier sessions
        # already deleted — a half-purge that no dry run predicted.
        for k in sorted(targets):
            d=mb.base/"sessions"/k
            if not os.path.lexists(d): continue
            if reparse(d): raise SystemExit(f"refusing to purge a linked target: {d}")
            check_deletable(d)
        ptr=mb.base/"active.json"
        # lexists, not exists: a broken symlink fails exists() while very much
        # being present, and it is also precisely the shape we refuse to follow.
        ptr_exists=os.path.lexists(ptr)
        if ptr_exists and reparse(ptr): raise SystemExit(f"refusing to purge around a linked pointer: {ptr}")
        # Presence, not parsed truthiness. `readj(...) or {}` treated an empty
        # object, a `null`, or an unreadable pointer as absent, so `last` went
        # true and the ignore rule was removed while the pointer survived.
        # An unreadable pointer is still a pointer. Parseability only decides
        # whether we can tell it names a session being removed; presence alone
        # decides whether the mailbox still holds state. Letting the decode error
        # escape turned "there is something here I cannot read" into a traceback.
        cur=None
        if ptr_exists:
            try: cur=readj(ptr)
            except (json.JSONDecodeError,UnicodeDecodeError,OSError): cur=None
        drop_ptr=isinstance(cur,dict) and cur.get("session_id") in targets
        # No-VCS mode never wrote an ignore rule, so there is none to find, none
        # to share with a linked worktree, and none to remove. The whole branch
        # is skipped rather than made to answer about a file that does not exist.
        novcs=mailbox_vcs(repo)=="none"
        if not novcs:
            ex=exclude_path(repo)
            if os.path.lexists(ex) and reparse(ex):
                raise SystemExit(f"info/exclude is a link ({ex}); refusing to purge, because removing the rule would rewrite whatever it points at")
        owned_lines,unowned_lines=([],[]) if novcs else exclude_state(repo); marked=bool(owned_lines)
        # `last` decides whether the shared ignore rule goes, so it must mean
        # "this mailbox is finished", not "the sessions I indexed are gone".
        # Anything still living here keeps the rule — including a pointer we are
        # not removing, even one naming nothing we recognise: a dangling
        # active.json is retained state, and ignoring it would drop the rule while
        # the container survived.
        # VCS_FILE is this tool's own bookkeeping, not retained user state. Left
        # in the stray list it would make `last` permanently false, and the
        # container would survive every complete teardown.
        stray=[q.name for q in mb.base.iterdir() if q.name not in ("sessions",".state.lock","active.json",VCS_FILE)]
        if ptr_exists and not drop_ptr: stray.append("active.json")
        plan=[f"  purge  {k}  ({v.get('status')}, work={v.get('work_unit')})" for k,v in sorted(targets.items())]
        plan+=[f"  keep   {k}  ({st})" for k,st in keep]
        if drop_ptr: plan.append("  clear  active.json (it points at a session being purged)")
        last = not keep and not stray
        # Decide the shared-rule question HERE, not at apply time, or the dry run
        # promises a removal that apply then declines — a preview that does not
        # preview. One decision, reported and then executed.
        shared=sharing_worktrees(repo) if (last and marked) else []
        if stray: plan.append(f"  keep   {PAIR_DIR}/ still holds state that is not being removed: {', '.join(sorted(stray)[:5])}")
        # Each candidate gets its own line. "The ignore rule" was ambiguous the
        # moment a file could hold two.
        if last and marked and not shared: plan.append(f"  remove this tool's marker/rule pair at info/exclude {at_lines(owned_lines)}")
        elif last and marked and shared: plan.append(f"  KEEP this tool's marker/rule pair at {at_lines(owned_lines)} — info/exclude is shared with "+", ".join(str(x) for x in shared[:3]))
        elif not last and marked: plan.append(f"  keep this tool's marker/rule pair at {at_lines(owned_lines)}; sessions remain")
        if unowned_lines:
            plan.append(f"  KEEP the ignore rule at info/exclude {at_lines(unowned_lines)}"
                        +" — not the exact line this tool writes, so it may be yours")
        if last: plan.append(f"  remove {PAIR_DIR}/ if nothing else remains in it")
        # Said rather than left to inference. The Git wording above is absent in
        # this mode, and silence would read as "the rule was handled".
        if novcs: plan.append("  n/a    no ignore-rule bookkeeping: this mailbox has no version control, so none was ever written")
        if not a.apply:
            print("DRY_RUN (nothing was changed; re-run with --apply)"); print("\n".join(plan)); return 0
        # Pointer first. `active()` dereferences it, so a crash between the two
        # leaves session directories with no pointer, which reads as a clean "no
        # active session". The other order would leave the pointer naming a
        # directory that is gone — a dangling reference every command follows.
        if drop_ptr:
            try: (mb.base/"active.json").unlink()
            except FileNotFoundError: pass
        for k in sorted(targets):
            d=mb.base/"sessions"/k
            if not os.path.lexists(d): continue
            # Re-ask at delete time. A target swapped for a link after preflight
            # is the whole reason the no-follow rule exists, and `is_dir()` here
            # would follow it. A link to a *file* must refuse too, not be quietly
            # skipped while teardown carries on and declares the mailbox finished.
            if reparse(d): raise SystemExit(f"{d} became a link after preflight; nothing further was deleted")
            if not d.is_dir(): raise SystemExit(f"{d} is no longer a directory; nothing further was deleted")
            rmtree_no_links(d)
        removed_rule=False
        if last and marked:
            # info/exclude can be shared: a linked worktree resolves it through
            # GIT_COMMON_DIR, so "this tree has no mailbox left" is not "no tree
            # has one". Removing the rule here would un-ignore a sibling
            # worktree's live mailbox.
            if shared:
                print(f"KEPT_IGNORE_RULE this tool's marker/rule pair at info/exclude {at_lines(owned_lines)} — shared with "
                      +", ".join(str(o) for o in shared[:3])
                      +"; another worktree may still have a mailbox, so it was left in place")
            else:
                remove_exclude(repo); removed_rule=True
    if last:
        # Outside the lock: the lock file itself lives in the container, so the
        # emptiness test can only be honest once it is gone. Drop the now-empty
        # sessions/ directory first — leaving it behind kept the container alive
        # and made a complete teardown look like a partial one.
        try:
            sd=mb.base/"sessions"
            if not reparse(sd) and sd.is_dir() and not any(sd.iterdir()): sd.rmdir()
            # The backend marker goes with the container, and only with it: it is
            # what tells the next command which mode this mailbox is in, so it
            # must outlive every session that is not the last one.
            vf=mb.base/VCS_FILE
            if os.path.lexists(vf) and not reparse(vf) and not any(q for q in mb.base.iterdir() if q.name!=VCS_FILE): vf.unlink()
            if not reparse(mb.base) and not any(mb.base.iterdir()): mb.base.rmdir()
        except (FileNotFoundError,OSError): pass
    print(f"PURGED sessions={len(targets)} kept={len(keep)}"+(f" ignore_rules_removed={len(owned_lines)}" if removed_rule else ""))
    if novcs: print("NO_IGNORE_RULE this mailbox has no version control, so none was ever written and none was removed")
    # The run names every candidate it kept, not only the ones it removed —
    # otherwise "reports every rule it found" was true of the preview and false
    # of the thing that actually happened.
    if marked and not removed_rule and not shared:
        print(f"KEPT_IGNORE_RULE this tool's marker/rule pair at info/exclude {at_lines(owned_lines)} — sessions remain, so it is still needed")
    if unowned_lines:
        print(f"KEPT_IGNORE_RULE info/exclude {at_lines(unowned_lines)} holds /{PAIR_DIR}/ but not the exact line this tool writes; it may be yours, so it was left alone")
    return 0

def exclude_state(repo):
    """(line indices of pairs this tool wrote, line indices of rules it did not).

    Both lists, because a file can contain any number of each and every caller
    has to say what it did with all of them."""
    p=exclude_path(repo)
    if not os.path.lexists(p): return [],[]
    lines=p.read_text(encoding="utf-8",errors="replace").splitlines()
    rule="/"+PAIR_DIR+"/"
    # Both kinds are reported, independently, because a file can hold both and
    # the caller has to describe each. Returning only the first match meant a
    # user's indented rule shadowed the exact one this tool had added below it —
    # the purge kept both and leaked ours — and later, once that was fixed, it
    # meant a preserved user rule went unmentioned while we announced removing
    # "the" rule, as if there were only one.
    owned=[]; unowned=[]
    for i in range(len(lines)):
        if i and lines[i-1].rstrip(chr(13)+chr(10)) in TOOL_MARKS and lines[i]==rule:
            owned.append(i)
        elif lines[i].strip()==rule:
            # Ownership needs the exact marker AND the exact line we write: a
            # rule edited even by whitespace is no longer the one the marker
            # vouches for. Not pedantry — a leading space stops Git honouring
            # the rule at all, while a trailing one does not (verified with
            # `git check-ignore`), so "whitespace-only" can be the difference
            # between an ignored mailbox and a tracked one.
            unowned.append(i)
    # Every candidate, because `remove_exclude` removes every owned pair and the
    # caller promises to report each rule it found. Recording only the first of
    # each made that promise false the moment a file held two.
    return owned,unowned

def remove_exclude(repo):
    """Drop our marked rule from info/exclude, atomically, preserving the rest byte for byte.

    This is the user's file. Rewriting it line by line normalized their newline style
    and their final-newline choice as a side effect of deleting two lines we
    added, so the edit works on raw bytes, keeps each surviving line's own
    terminator, and lands through a temp file and a rename so an interruption
    cannot truncate it."""
    p=exclude_path(repo); raw=p.read_bytes(); lines=raw.splitlines(keepends=True)
    def bare(b): return b.decode("utf-8",errors="replace").strip()
    # Both lines are compared exactly, stripping only the terminator. `.strip()`
    # handed a user line that merely wrapped our text in spaces to the purge as
    # tool-written — and the same laxity on the rule claimed a rule the user had
    # edited. Whitespace on the rule is not cosmetic either: a leading space
    # stops Git honouring it, so a "whitespace-only" edit can be the difference
    # between an ignored mailbox and a tracked one.
    def marker_line(b): return b.decode("utf-8",errors="replace").rstrip(chr(13)+chr(10))
    keep=[]; i=0
    while i<len(lines):
        if marker_line(lines[i]) in TOOL_MARKS and i+1<len(lines) and marker_line(lines[i+1])=="/"+PAIR_DIR+"/":
            # We supplied the separator newline on a file that had none, so give
            # it back: strip the terminator from what is now the last line.
            if NO_EOL_NOTE in bare(lines[i]) and keep:
                keep[-1]=keep[-1].rstrip(bytes([13,10]))
            i+=2; continue
        keep.append(lines[i]); i+=1
    mode=p.stat().st_mode
    fd,tmp=tempfile.mkstemp(prefix=p.name+".",suffix=".tmp",dir=p.parent)
    try:
        with os.fdopen(fd,"wb") as f: f.write(b"".join(keep)); f.flush(); os.fsync(f.fileno())
        # mkstemp creates 0600. Replacing the user's file with it would silently
        # tighten (or otherwise change) their permissions as a side effect of
        # deleting two lines.
        # Fail closed. Swallowing this installed mkstemp's 0600 over the user's
        # file and still reported success — a permissions change presented as a
        # two-line deletion.
        try: os.chmod(tmp,mode & 0o7777)
        except OSError as e: raise SystemExit(f"cannot preserve the mode of {p} ({e}); info/exclude was left unchanged")
        if p.read_bytes()!=raw: raise SystemExit("info/exclude changed while it was being edited; nothing was written")
        os.replace(tmp,p)
    finally:
        try: os.unlink(tmp)
        except FileNotFoundError: pass

def park(a):
    mb=MB(root(a.repo))
    with Lock(mb.lock):
        s=require(mb,a.role,allow_paused=True)
        # Remember which status to come back to. Parking a paused session and
        # resuming it as active would silently restore write access that a peer's
        # quota state was withholding — invariant 8, no automatic solo fallback.
        s["parked_from"]=s["status"]; s["status"]="parked"; s["parked_at"]=now()
        s["parked_tree"]=snap(root(a.repo)); s["park_reason"]=getattr(a,"reason",None)
        s["parked_companions"]={c["name"]:snap(Path(c["root"]),c.get("vcs")) for c in (s.get("companions") or [])}
        s["updated_at"]=now(); mb.save(s)
    print(f"PARKED session={s['session_id']} work={s['work_unit']} from={s['parked_from']}"); return 0

def resume(a):
    repo=root(a.repo); mb=MB(repo); live=snap(repo)
    # Everything that decides WHICH session becomes active happens under the one
    # lock. Checking outside it let a concurrent `start` pass its own locked
    # check, publish session B, and then be silently overwritten here — B
    # orphaned, and the active-session refusal bypassed.
    with Lock(mb.lock):
        cur=mb.active()
        if cur and cur["status"] not in ("completed","abandoned","parked"):
            raise SystemExit(f"active session {cur['session_id']} ({cur['work_unit']}) must be parked or closed first")
        parked=parked_sessions(mb)
        if not parked: raise SystemExit("no parked session")
        if a.session:
            # Membership in the scanned map, never a path built from the argument,
            # so an unknown id and a traversal-shaped one fail the same way.
            if a.session not in parked: raise SystemExit(f"unknown parked session {a.session!r}")
            s=parked[a.session]
        elif len(parked)==1: s=next(iter(parked.values()))
        else:
            raise SystemExit("ambiguous; pass --session <id>:\n"+"\n".join(
                f"  {k} {v['work_unit']} parked_at={v.get('parked_at')}" for k,v in parked.items()))
        check_protocol(s,f"parked session {s.get('session_id')}")
        before=s.get("parked_tree") or {}
        # Pointer first, while the target still reads parked. MB.save writes
        # session.json before active.json, so reactivating with it would leave a
        # window where the target says "active" and the pointer still names the
        # old session — an active session nothing can reach, and one the parked
        # scan no longer finds either. Writing the pointer first inverts the
        # failure: a crash here leaves a visible SESSION_PARKED that resume
        # retries cleanly. `active()` reads status through to session.json, so
        # the pointer's own copy is only metadata.
        mb.point(s,status="parked")
        s["status"]=s.pop("parked_from","active"); s.pop("parked_at",None); s.pop("parked_tree",None); settle_status(s)
        comp_before=s.pop("parked_companions",None) or {}
        # Every resume invalidates review standing, not only one that detects a
        # changed tree: a rule that depends on detecting the change can fail to
        # detect it, and the failure mode is an AGREE from before the park
        # authorizing a close over whatever the other task did.
        s["phase"]="implement"
        s["verdict_floor"]={r:int((readj(mb.latest(s,r),{}) or {}).get("seq",0)) for r in participants(s)}
        # The parked interval is not peer silence; leaving `since` alone would fire
        # PEER_STALE the moment the session comes back.
        if s.get("waiting"):
            if schema(s)==1: s["waiting"]["since"]=now()
            else:
                wm=waiting_map(s)
                for e in wm.values(): e["since"]=now()
                s["waiting"]=wm
        s["updated_at"]=now(); mb.save(s)
    print(f"RESUMED session={s['session_id']} status={s['status']} work={s['work_unit']}")
    if len(participants(s))>2:
        for r,pz in sorted(pauses(s).items()): print(pause_line(s,r,pz))
    print("REVIEW_INVALIDATED phase=implement; a new peer AGREE is required before complete")
    if live!=before:
        # protocol.md promises a HEAD *and* status delta. Reporting only the head
        # printed "head X -> X" when the change was entirely in the working tree,
        # which reads as no change at all.
        parts=[]
        if live.get("head")!=before.get("head"): parts.append(f"head {before.get('head')} -> {live.get('head')}")
        was=set(before.get("status") or []); is_=set(live.get("status") or [])
        def brief(x): return ", ".join(sorted(x)[:5])+(" ..." if len(x)>5 else "")
        if is_-was: parts.append(f"now: {brief(is_-was)}")
        if was-is_: parts.append(f"no longer: {brief(was-is_)}")
        print("TREE_CHANGED "+"; ".join(parts))
    # The same report per companion. Resume invalidates review standing
    # unconditionally, so this is information rather than the trigger - but a
    # plan record that moved while the session was parked is exactly what the
    # returning pair must know to re-read.
    for c in (s.get("companions") or []):
        was=comp_before.get(c["name"]); live=snap(Path(c["root"]),c.get("vcs"))
        if was and live!=was:
            print(f"COMPANION_CHANGED {c['name']} head {was.get('head')} -> {live.get('head')} dirty {len(was.get('status') or [])} -> {len(live.get('status') or [])}")
    return 0

def cmd_post(a):
    mb=MB(root(a.repo)); b=body(a)
    # Addressing is validated by `MB.post` on the snapshot it appends under.
    # It is checked here too, first, only so that a refused post leaves no
    # acknowledgement behind.
    s0=mb.active()
    if s0 and a.role in participants(s0):
        address(mb,s0,a.role,a.kind,a.expect_reply,a.to,a.reply_to,a.broadcast,forward=a.forward)
    # Validated before the ack, so a post refused for its body cannot leave an
    # acknowledgement behind that no reply ever accompanied.
    if not b.strip(): raise SystemExit("refusing to post an empty message; a body that failed to render is worse than no post, because the peer treats it as a real turn")
    if a.ack_through is not None:
        with Lock(mb.lock): apply_ack(mb,require(mb,a.role,allow_paused=True),a.role,a.ack_through)
    m=mb.post(a.role,a.kind,b,a.expect_reply,to=a.to,reply_to=a.reply_to,broadcast=a.broadcast,forward=a.forward)
    # stderr, so a successful post stays silent on stdout. Not an ack: a post
    # proves the model is active, not that it saw what some watcher printed.
    s=mb.active()
    if s and s["session_id"]:
        if schema(s)==1:
            r=unacked_range(mb,s,a.role)
            if r: print(f"UNACKED peer #{r[0]}..#{r[1]} (post --ack-through N or `ack --through N` once read)",file=sys.stderr)
        else:
            u=unacked_v2(mb,s,a.role)
            if u: print("UNACKED "+",".join(f"{snd}:{x}..{snd}:{y}" for snd,x,y in u)
                        +" (post --ack-through sender:N[,sender:N] or `ack --through ...` once read)",file=sys.stderr)
    return 0

def ack(a):
    mb=MB(root(a.repo))
    with Lock(mb.lock): out=apply_ack(mb,require(mb,a.role,allow_paused=True),a.role,a.through)
    print(out); return 0

def abandon(a):
    mb=MB(root(a.repo))
    with Lock(mb.lock):
        s=require(mb,a.role,allow_paused=True); s["status"]="abandoned"; s["waiting"]=None
        s["abandon_reason"]=getattr(a,"reason",None); s["abandoned_at"]=now(); s["updated_at"]=now(); mb.save(s)
    print(f"ABANDONED session={s['session_id']}"+(f" reason={s['abandon_reason']}" if s["abandon_reason"] else "")); return 0

def transcript(a):
    mb=MB(root(a.repo))
    if getattr(a,"session",None):
        base=mb.base/"sessions"
        known={d.name for d in base.iterdir()} if base.is_dir() else set()
        if a.session not in known: raise SystemExit(f"unknown session {a.session!r}")
        s=readj(session_file(base/a.session))
        if not s: raise SystemExit(f"unknown session {a.session!r}")
        check_protocol(s,f"session {a.session}")
    else:
        s=mb.active()
        if not s: raise SystemExit("NO_ACTIVE_SESSION")
    arr=[]
    for r in participants(s):
        p=mb.journal(s,r)
        if p.exists():
            arr.extend(journal_records(p))
    if len(participants(s))>2:
        for u in (s.get("units") or []):
            if u.get("authority"): print(authority_line(u.get("owner"),u["authority"],unit_label(u.get("work_unit"),u.get("unit_id"))))
        print(authority_line(s["owner"],authority(s),unit_label(s.get("work_unit"),s.get("unit_id"))))
        print()
    # Every message carries the work unit it was written under. Dropping it here
    # made a multi-unit transcript unattributable - the one place the history is
    # meant to be readable back.
    for m in sorted(arr,key=lambda x:(x.get("at",""),x.get("role",""),x.get("seq",0))):
        unit=f" [{unit_label(m.get('work_unit'),m.get('unit_id'))}]" if m.get("work_unit") else ""
        mk=route_marks(m) if m.get("msg_id") else ""
        print(f"## {m['at']} {m['role']} #{m['seq']} {m['kind']}{unit}{mk}\n{m.get('body','')}\n")
    # The closing record, after the mail it closed, for the same reason `park`
    # keeps its reason: a transcript read later must say why the work stopped.
    if s.get("status")=="abandoned" and s.get("abandon_reason"):
        print(f"## {s.get('abandoned_at') or s.get('updated_at')} ABANDONED\n{s['abandon_reason']}\n")
    return 0

def parser():
    p=argparse.ArgumentParser(); p.add_argument("--repo"); sp=p.add_subparsers(dest="cmd",required=True)
    def rc(n): q=sp.add_parser(n); q.add_argument("--role",choices=PARTICIPANTS,required=True); return q
    q=rc("start"); q.add_argument("--task",dest="body"); q.add_argument("--task-file",dest="body_file"); q.add_argument("--work-unit")
    q.add_argument("--with",dest="with_",help="the other participant(s), comma-separated; default: the historic claude/codex partner")
    q.add_argument("--advisers",help="non-owner participant(s) who advise but need not agree; default: none, so every non-owner is a required reviewer")
    # Only `start` takes it. The mode is a property of the mailbox from then on,
    # so the navigator's `join` and every later command read it rather than being
    # told again — and cannot be told a different one.
    q.add_argument("--no-vcs",action="store_true",help="anchor ownership to a content digest of this directory instead of Git HEAD; refused inside a Git working tree")
    q.set_defaults(fn=start)
    q=rc("join"); q.add_argument("--timeout",type=int,default=0); q.add_argument("--poll",type=float,default=1); q.set_defaults(fn=join)
    q=rc("post"); q.add_argument("--kind",choices=sorted(KINDS),required=True); q.add_argument("--body"); q.add_argument("--body-file"); q.add_argument("--expect-reply",action="store_true")
    q.add_argument("--ack-through",help="acknowledge peer mail through N, or sender:N[,sender:N] with three participants")
    q.add_argument("--to",help="recipient(s), comma-separated; required with three participants unless replying")
    q.add_argument("--reply-to",help="the msg_id (<role>:<seq>) this answers; the reply goes to its author")
    q.add_argument("--broadcast",action="store_true",help="send to every participant (ESCALATE only)")
    q.add_argument("--forward",action="store_true",help="pass the --reply-to message on to --to; spends one forward edge")
    q.set_defaults(fn=cmd_post)
    for n,b in (("watch",True),("peek",False)):
        q=rc(n); q.add_argument("--timeout",type=int,default=0); q.add_argument("--poll",type=float,default=1); q.add_argument("--stale-after",type=int,default=900)
        q.add_argument("--ack-through",help="acknowledge through N, or sender:N[,sender:N], before waiting"); q.set_defaults(fn=lambda a,x=b:watch(a,x))
    q=rc("ack"); q.add_argument("--through",required=True,help="N, or sender:N[,sender:N]"); q.set_defaults(fn=ack)
    q=rc("health"); q.add_argument("--status",choices=sorted(HEALTH),required=True); q.add_argument("--reason"); q.add_argument("--resume-at"); q.set_defaults(fn=health)
    q=sp.add_parser("status"); q.set_defaults(fn=status)
    q=rc("guard-write"); q.add_argument("--path"); q.set_defaults(fn=guard)
    q=rc("endpoint"); g=q.add_mutually_exclusive_group(required=True); g.add_argument("--register",action="store_true"); g.add_argument("--unregister",action="store_true")
    q.add_argument("--transport"); q.add_argument("--address"); q.add_argument("--ttl",type=int,help="seconds until the registration expires"); q.set_defaults(fn=endpoint)
    q=rc("accept"); q.add_argument("--msg-id",required=True); q.add_argument("--generation",type=int,required=True); q.set_defaults(fn=accept)
    q=rc("record-push"); q.add_argument("--msg-id",required=True); q.add_argument("--to",required=True); q.add_argument("--generation",type=int,required=True)
    q.add_argument("--outcome",choices=["sent","refused","failed","timeout"],required=True); q.set_defaults(fn=record_push)
    q=rc("handoff-offer"); q.add_argument("--to",help="the incoming owner; required with three participants"); q.set_defaults(fn=handoff_offer)
    q=rc("handoff-accept"); q.set_defaults(fn=handoff_accept)
    q=rc("phase"); q.add_argument("--phase",choices=("huddle","implement","review","blocked","complete"),required=True); q.set_defaults(fn=phase)
    q=rc("verify-request"); q.add_argument("--criteria"); q.add_argument("--criteria-file"); q.add_argument("--check",action="append"); q.add_argument("--file",action="append"); q.add_argument("--round",default="1"); q.set_defaults(fn=verify_request)
    q=rc("next-unit"); q.add_argument("--task",dest="body"); q.add_argument("--task-file",dest="body_file"); q.add_argument("--work-unit")
    q.add_argument("--advisers",help="advisers for the new unit, or 'none'; default: the current unit's advisers")
    q.set_defaults(fn=next_unit)
    q=rc("complete"); q.set_defaults(fn=complete)
    q=rc("abandon"); q.add_argument("--reason"); q.set_defaults(fn=abandon)
    q=rc("purge"); g=q.add_mutually_exclusive_group(); g.add_argument("--session"); g.add_argument("--all-closed",action="store_true"); q.add_argument("--apply",action="store_true"); q.set_defaults(fn=purge)
    q=rc("park"); q.add_argument("--reason"); q.set_defaults(fn=park)
    q=rc("resume"); q.add_argument("--session"); q.set_defaults(fn=resume)
    q=sp.add_parser("transcript"); q.add_argument("--session"); q.set_defaults(fn=transcript)
    return p
def select_anchor(a):
    """Pick where this command runs: `--repo`, then `PAIR_REPO`, then the cwd.

    A stale `PAIR_REPO` fails here, loudly, and never falls back to the cwd:
    silently running against a different tree than the one named is worse than
    stopping. Only `start` may name a directory with no mailbox yet."""
    if a.repo: a.anchor_source="flag"; return
    env=os.environ.get("PAIR_REPO")
    if not env: a.anchor_source="cwd"; return
    a.anchor_source="env"; q=Path(env).expanduser()
    if not q.is_dir(): raise SystemExit(f"PAIR_REPO={env} is not a directory; fix or unset it (no fallback to the current directory)")
    if a.cmd!="start" and find_mailbox(q.resolve()) is None:
        raise SystemExit(f"PAIR_REPO={env} has no pair mailbox; fix or unset it (no fallback to the current directory)")
    a.repo=str(q)

def anchor_line(a):
    return f"ANCHOR={root(a.repo)} source={getattr(a,'anchor_source','flag')}"

if __name__=="__main__":
    a=parser().parse_args()
    try:
        try: select_anchor(a); raise SystemExit(a.fn(a) or 0)
        except json.JSONDecodeError as e:
            # A record that exists but cannot be read is refused, never read as
            # absent (spec L-8).
            raise SystemExit(f"corrupt mailbox: {e.msg}; refusing to continue") from None
    except SystemExit as e:
        # Every error names the tree it ran against, so a stale anchor is
        # visible at the moment it causes a failure rather than one step later.
        if isinstance(e.code,str) and "PAIR_REPO=" not in e.code:
            raise SystemExit(f"{e.code} [anchor={a.repo or os.getcwd()} source={getattr(a,'anchor_source','cwd')}]")
        raise
