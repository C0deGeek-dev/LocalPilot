# LocalX compatibility shortcuts for the native LocalBox and LocalBench CLIs.
# This file is managed by `localx install powershell-shortcuts`.

function llm { localbox @args }

function llm-add {
    # With no repository, open the guided launcher where Add Model can be
    # selected. An explicit repo/URL uses LocalBox's scripted register+download
    # path and accepts the same --quant option as `localbox download`.
    if ($args.Count -eq 0) {
        localbox
    }
    else {
        localbox download @args
    }
}

function llm-update { localbox update @args }
function llmlaunch { localbox launch @args }
function llmserve { localbox serve @args }
function llmstop { localbox stop @args }
function llmstatus { localbox status @args }
function llminfo { localbox info @args }
function llmlog { localbox log @args }
function llmtune { localbench findbest @args }
