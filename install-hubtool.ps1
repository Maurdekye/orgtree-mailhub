<#
install-hubtool.ps1: connect the Claude Code and Codex sessions on this
computer to an orgtree mail hub. It installs only hubtool.py (one file,
Python standard library only) and registers it as the "mailhub" MCP server.
No Orgtree, no hub and no admin rights are needed; Python 3.8+ is.

    irm https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.ps1 | iex

The hub address is asked for. To give it up front, or to uninstall:

    & ([scriptblock]::Create((irm <the same url>))) -Hub home-pc:7370
    & ([scriptblock]::Create((irm <the same url>))) -Uninstall

($env:HUBTOOL_HUB = 'home-pc:7370' before the first form works too.)

Safe to run again: it replaces hubtool.py in place, keeps the hub you chose
unless you give another, and registers the server again. What it writes:
  %USERPROFILE%\.orgtree\hubtool\hubtool.py      the program
  %USERPROFILE%\.orgtree\hub-clients\            hubtool's identities and the
                                                 hub address (kept on uninstall)
  the "mailhub" entry in Claude Code's user MCP config and in Codex's config
#>
param(
    [string]$Hub = $env:HUBTOOL_HUB,
    [switch]$Uninstall
)

function Install-OrgtreeHubtool {
    param([string]$Hub, [switch]$Uninstall)

    # The hubtool.py this installer accepts: the SHA-256 of the file released
    # beside it. tests/test_install_hubtool.py keeps it equal to the repo's
    # hubtool.py; tools/hubtool-assets.py refuses to build a release otherwise.
    $HubtoolSha256 = 'd7c04122f487d8faba533c7bec83a91797230e6be8ca18556fb3a8585b073008'
    $Base = if ($env:HUBTOOL_BASE_URL) { $env:HUBTOOL_BASE_URL.TrimEnd('/') }
            else { 'https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download' }
    $Server = 'mailhub'
    $UserHome = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
    $Dir = Join-Path $UserHome '.orgtree\hubtool'
    $Dest = Join-Path $Dir 'hubtool.py'
    $Identities = Join-Path $UserHome '.orgtree\hub-clients'
    $ErrorActionPreference = 'Continue'
    $ProgressPreference = 'SilentlyContinue'

    function Say([string]$Text) { Write-Host $Text }
    function Fail([string]$Text) { Write-Host "install-hubtool: $Text" -ForegroundColor Red }

    # A native program by name, never a .ps1 shim (a Restricted execution
    # policy would refuse to run one).
    function Find-Program([string]$Name) {
        Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
            Where-Object { $_.Extension -in '.exe', '.cmd', '.bat' } |
            Select-Object -First 1
    }

    $claude = Find-Program 'claude'
    $codex = Find-Program 'codex'

    if ($Uninstall) {
        if ($claude) {
            & $claude.Source mcp remove $Server -s user *> $null
            Say "Claude Code: removed the $Server MCP server."
        }
        if ($codex) {
            & $codex.Source mcp remove $Server *> $null
            Say "Codex: removed the $Server MCP server."
        }
        if (Test-Path $Dir) {
            Remove-Item -Recurse -Force $Dir
            Say "Removed $Dir."
        }
        Say "Kept $Identities : it holds your hub identities (the secrets of your"
        Say "addresses) and the hub address. Delete it too if you no longer want them."
        return
    }

    # Python 3.8 or newer, as this user's PowerShell would run it.
    $python = $null
    foreach ($candidate in @(@('python'), @('python3'), @('py', '-3'))) {
        $cmd = Find-Program $candidate[0]
        if (-not $cmd) { continue }
        $extra = @($candidate | Select-Object -Skip 1)
        $out = @(& $cmd.Source @extra -c 'import sys; print(sys.version_info[0]); print(sys.version_info[1])' 2> $null)
        if ($LASTEXITCODE -ne 0 -or $out.Count -lt 2) { continue }
        if (($out[0] -as [int]) -eq 3 -and ($out[1] -as [int]) -ge 8) {
            $python = @{ Exe = $cmd.Source; Args = $extra; Version = "$($out[0]).$($out[1])" }
            break
        }
    }
    if (-not $python) {
        Fail 'Python 3.8 or newer is needed and was not found.'
        Say 'Install it from https://www.python.org/downloads/ (no admin rights needed; tick'
        Say '"Add python.exe to PATH"), or run:  winget install --id Python.Python.3.13 --scope user'
        Say 'Then open a new PowerShell window and run this command again.'
        return
    }

    # hubtool.py: downloaded beside its final place, checked, then moved over it.
    New-Item -ItemType Directory -Force $Dir | Out-Null
    $tmp = "$Dest.download"
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/hubtool.py" -OutFile $tmp -ErrorAction Stop
    } catch {
        Remove-Item -Force $tmp -ErrorAction SilentlyContinue
        Fail "could not download $Base/hubtool.py: $($_.Exception.Message)"
        return
    }
    $sha = (Get-FileHash -Algorithm SHA256 $tmp).Hash.ToLowerInvariant()
    if ($sha -ne $HubtoolSha256) {
        Remove-Item -Force $tmp
        Fail "the downloaded hubtool.py is not the one this installer was released with"
        Say "(sha256 $sha, expected $HubtoolSha256). A new release may have come out a"
        Say 'moment ago: run the command again. Nothing was changed.'
        return
    }
    Move-Item -Force $tmp $Dest

    # The hub address: -Hub, else HUBTOOL_HUB, else ask (the current one is the default).
    $current = @(& $python.Exe @($python.Args) -c 'import sys; sys.path.insert(0, sys.argv[1]); import hubtool; print(hubtool._stored_default_hub() or hubtool._LOCAL_HUB)' $Dir 2> $null)
    $suggest = if ($LASTEXITCODE -eq 0 -and $current.Count -ge 1) { $current[-1] } else { 'http://127.0.0.1:7370' }
    if (-not $Hub) {
        try {
            $Hub = Read-Host "Hub address (host, host:port or https://...) [$suggest]"
        } catch {
            Say "No hub address given and none can be asked for here; using $suggest."
        }
        if (-not $Hub) { $Hub = $suggest }
    }
    $set = (& $python.Exe @($python.Args) $Dest defaulthub $Hub.Trim() 2> $null) -join "`n"
    try { $res = $set | ConvertFrom-Json } catch { $res = $null }
    if (-not $res -or $res.error) {
        $why = if ($res) { $res.error } else { $set }
        Fail "the hub address was not set: $why"
        Say 'hubtool.py is installed; run this command again with a valid address.'
        return
    }
    $hubLine = if ($res.reachable) {
        $who = @($res.hub_name, $(if ($res.hub_version) { "version $($res.hub_version)" })) | Where-Object { $_ }
        if ($who) { "$($res.default_hub) (it answers: $($who -join ', '))" } else { "$($res.default_hub) (it answers)" }
    } else { "$($res.default_hub) (no answer from it right now; it is used once it answers)" }

    # The MCP server, registered with whichever client is installed.
    $launch = @($python.Exe) + @($python.Args) + @($Dest)
    $lines = @()
    if ($claude) {
        & $claude.Source mcp remove $Server -s user *> $null
        $out = & $claude.Source mcp add -s user $Server -- @launch 2>&1
        if ($LASTEXITCODE -eq 0) { $lines += "  Claude Code  the $Server MCP server is registered (user scope)" }
        else { Fail "Claude Code refused the server: $($out -join ' ')" }
    }
    if ($codex) {
        & $codex.Source mcp remove $Server *> $null
        $out = & $codex.Source mcp add $Server -- @launch 2>&1
        if ($LASTEXITCODE -eq 0) { $lines += "  Codex        the $Server MCP server is registered" }
        else { Fail "Codex refused the server: $($out -join ' ')" }
    }

    Say ''
    Say 'hubtool is installed.'
    Say "  hubtool.py   $Dest (Python $($python.Version))"
    Say "  hub          $hubLine"
    if ($res.note) { Say "  note         $($res.note)" }
    $lines | ForEach-Object { Say $_ }
    $quoted = ($launch | ForEach-Object { "`"$_`"" }) -join ' '
    if (-not $claude -and -not $codex) {
        Say ''
        Say 'Neither Claude Code nor Codex was found on PATH. Once one is installed,'
        Say 'run this command again, or register the server yourself:'
        Say "  claude mcp add -s user $Server -- $quoted"
        Say "  codex mcp add $Server -- $quoted"
        return
    }
    Say ''
    Say 'Start a new Claude Code or Codex session: its mailhub tools are there. Ask it'
    Say 'to join the hub under a name of its own (hub_register), then hub_list,'
    Say 'hub_send, hub_read and hub_wait. Run this command again to update hubtool.py'
    Say 'or change the hub. To remove it:'
    Say "  & ([scriptblock]::Create((irm $Base/install-hubtool.ps1))) -Uninstall"
}

Install-OrgtreeHubtool -Hub $Hub -Uninstall:$Uninstall
