from duration import parse_duration
ok={"1h30m":5400,"45s":45,"2h5s":7205,"0s":0,"10m":600,"1h1m1s":3661}
for s,v in ok.items(): assert parse_duration(s)==v,(s,parse_duration(s))
for bad in ("","1x","30m1h","1h1h"," 1h","1 h","-5s","h","1h30"):
    try: parse_duration(bad)
    except ValueError: pass
    else: raise AssertionError(f"no ValueError for {bad!r}")
print("HIDDEN_OK")
