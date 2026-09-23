#!/usr/bin/env python3
"""
Reman shell integration generator (cross-platform, like `atuin init` / `starship init`).

Emits the shell glue for the `reman` command + the inline pickers. Paths (this python +
reman_daemon.py) are auto-detected, so the SAME code works on Windows (PowerShell), macOS, and
Linux (zsh/bash) - only the emitted glue differs per shell.

  Windows PowerShell:  reman_init.py powershell | Out-String | Invoke-Expression
  macOS/Linux zsh:     eval "$(python3 /path/to/reman_init.py zsh)"     # in ~/.zshrc
  macOS/Linux bash:    eval "$(python3 /path/to/reman_init.py bash)"    # in ~/.bashrc

Keys: UpArrow = live history finder (type to semantic-filter, Tab toggles all/you/agent).
      Ctrl+R  = quick semantic search of what's already typed (arrow-select).
zsh/bash use fzf (requires fzf on PATH: brew/winget/apt install fzf).
"""
import sys, os

PY = sys.executable.replace("\\", "/")
DAEMON = os.path.join(os.path.dirname(os.path.abspath(__file__)), "reman_daemon.py").replace("\\", "/")

POWERSHELL = r'''
function reman { & "__PY__" "__DAEMON__" @args }

# Talk to the warm daemon directly over the socket (instant; no per-keystroke process spawn).
function __RemanSend {
  param($obj)
  try {
    $c = New-Object Net.Sockets.TcpClient; $c.Connect('127.0.0.1', 8765)
    $st = $c.GetStream()
    $b = [Text.Encoding]::UTF8.GetBytes(($obj | ConvertTo-Json -Compress) + "`n")
    $st.Write($b, 0, $b.Length); $st.Flush()
    $resp = (New-Object IO.StreamReader($st, [Text.Encoding]::UTF8)).ReadLine()
    $c.Close(); return ($resp | ConvertFrom-Json)
  } catch { return $null }
}

function __RemanFetch {
  param($query, $filter, $scope, $status)
  if ([string]::IsNullOrWhiteSpace($query)) { $req = @{ op = 'recent'; k = 0 } }   # browse: ALL commands (scroll the whole history)
  else { $req = @{ op = 'search'; query = $query; k = 40 } }                        # search: top-k ranked
  if ($scope) { $req['cwd'] = $scope }                               # restrict to this folder
  if ($filter -eq 'you') { $req['actor'] = 'human' }                 # actor filtered server-side, so
  elseif ($filter -eq 'agent') { $req['actor'] = 'agent' }           # it spans ALL commands, not top-k
  if ($status -eq 'ok' -or $status -eq 'fail') { $req['status'] = $status }   # pass/fail filter
  $r = __RemanSend $req
  $list = @()
  if ($r -and $r.results) {
    foreach ($x in $r.results) {
      $a = if ($x.actor) { [string]$x.actor } else { 'human' }
      $st = if ($x.status) { [string]$x.status } else { 'unknown' }
      $list += , @([string]$x.command, $a, $st)
    }
  }
  return $list
}

# Make sure the warm daemon is reachable. Fast path = a socket ping (ms). Only if that fails do we
# pay for a Python spawn (the python client auto-starts the daemon). Avoids a 1-2s freeze per open.
function __RemanEnsure {
  if (-not (__RemanSend @{ op = 'ping' })) { & "__PY__" "__DAEMON__" ping *> $null }
  __RemanSend @{ op = 'sync'; limit = 200 } | Out-Null   # pull new live commands from Atuin (bounded)
}

# Inline picker (PSReadLine 2.1+). $live=$true -> UpArrow finder (type/Tab); else Ctrl+R static.
# Returns @{ chosen=<cmd or $null>; promptY=<row to redraw the prompt at> }.
function __RemanPick {
  param($initialQuery, $live)
  __RemanEnsure
  $maxRows = 8; $headerRows = if ($live) { 1 } else { 0 }; $regionH = $maxRows + $headerRows
  $w = [Console]::WindowWidth
  $promptX = [Console]::CursorLeft; $promptY = [Console]::CursorTop
  $room = ([Console]::WindowTop + [Console]::WindowHeight - 1) - $promptY
  if ($room -lt $regionH) {
    $scroll = $regionH - $room
    [Console]::SetCursorPosition(0, [Console]::WindowTop + [Console]::WindowHeight - 1)
    for ($s = 0; $s -lt $scroll; $s++) { [Console]::WriteLine() }
    $promptY = $promptY - $scroll
  }
  $regionStart = $promptY + 1
  [Console]::CursorVisible = $false
  $query = $initialQuery; $filter = 'all'; $sel = 0
  $items = @(__RemanFetch $query $filter)
  $draw = {
    if ($live) {
      [Console]::SetCursorPosition(0, $regionStart)
      $hint = " Tab:[" + $filter + "]  type to search . Esc"
      $head = "  reman> " + $query
      $l2 = if (($head.Length + $hint.Length) -lt ($w - 1)) { $head.PadRight($w - 1 - $hint.Length) + $hint } else { $head }
      if ($l2.Length -gt $w - 1) { $l2 = $l2.Substring(0, $w - 1) }
      Write-Host $l2.PadRight($w - 1) -NoNewline -ForegroundColor Red
    }
    for ($i = 0; $i -lt $maxRows; $i++) {
      $rrow = $regionStart + $headerRows + $i
      [Console]::SetCursorPosition(0, $rrow); [Console]::Write(' ' * ($w - 1))   # clear row first
      [Console]::SetCursorPosition(0, $rrow)
      if ($i -lt $items.Count) {
        $cmd = ([string]$items[$i][0]) -replace '[\x00-\x1F\x7F]+', ' '   # strip control chars (newlines, BEL=beep, ESC) -> one clean row
        $actor = [string]$items[$i][1]; $status = [string]$items[$i][2]
        $mark = if ($i -eq $sel) { '> ' } else { '  ' }
        $gl = switch ($status) { 'ok' { '+' } 'fail' { 'x' } 'mixed' { '~' } default { ' ' } }
        $glc = switch ($status) { 'ok' { 'Green' } 'fail' { 'Red' } 'mixed' { 'Yellow' } default { 'DarkGray' } }
        Write-Host $mark -NoNewline -ForegroundColor Gray
        Write-Host ([string]$gl + ' ') -NoNewline -ForegroundColor $glc
        $maxc = $w - 17
        if ($cmd.Length -gt $maxc) { $cmd = $cmd.Substring(0, $maxc) }
        $cmd = $cmd.PadRight($maxc)
        if ($i -eq $sel) { Write-Host $cmd -NoNewline -ForegroundColor White -BackgroundColor DarkRed }
        else { Write-Host $cmd -NoNewline -ForegroundColor Gray }
        $atag = (' ' + $actor); if ($atag.Length -gt 12) { $atag = $atag.Substring(0, 12) }
        $ac = if ($actor -like 'agent*') { 'DarkYellow' } else { 'DarkGreen' }
        Write-Host $atag.PadRight(12) -NoNewline -ForegroundColor $ac
      }
    }
  }
  & $draw
  $chosen = $null; $done = $false
  while (-not $done) {
    $k = [Console]::ReadKey($true)
    if ($k.Key -eq 'Enter') { if ($items.Count -gt 0) { $chosen = [string]$items[$sel][0] }; $done = $true }
    elseif ($k.Key -eq 'Escape') { $done = $true }
    elseif ($k.Key -eq 'UpArrow') { if ($items.Count) { $sel = ($sel - 1 + $items.Count) % $items.Count } }
    elseif ($k.Key -eq 'DownArrow') { if ($items.Count) { $sel = ($sel + 1) % $items.Count } }
    elseif ($live -and $k.Key -eq 'Tab') { $filter = switch ($filter) { 'all' { 'you' } 'you' { 'agent' } default { 'all' } }; $sel = 0; $items = @(__RemanFetch $query $filter) }
    elseif ($live -and $k.Key -eq 'Backspace') { if ($query.Length -gt 0) { $query = $query.Substring(0, $query.Length - 1); $sel = 0; $items = @(__RemanFetch $query $filter) } }
    elseif ($live -and $k.KeyChar -and -not [char]::IsControl($k.KeyChar)) { $query += $k.KeyChar; $sel = 0; $items = @(__RemanFetch $query $filter) }
    elseif (-not $live -and [char]::IsDigit($k.KeyChar)) { $num = [int]([string]$k.KeyChar); if ($num -ge 1 -and $num -le $items.Count) { $chosen = [string]$items[$num - 1][0]; $done = $true } }
    & $draw
  }
  for ($i = 0; $i -lt $regionH; $i++) { [Console]::SetCursorPosition(0, $regionStart + $i); [Console]::Write(' ' * ($w - 1)) }
  [Console]::SetCursorPosition($promptX, $promptY)
  [Console]::CursorVisible = $true
  return @{ chosen = $chosen; promptY = $promptY }
}

# Legacy PSReadLine 2.0.x fallback: numbered picker printed as normal output (+ upgrade tip).
function __RemanNumbered {
  param($query)
  if ([string]::IsNullOrWhiteSpace($query)) { $raw = @(& "__PY__" "__DAEMON__" recentfull 2>$null) }
  else { $raw = @(& "__PY__" "__DAEMON__" completefull $query 2>$null) }
  $items = @($raw | Where-Object { $_ } | Select-Object -First 9 | ForEach-Object { , ($_ -split ([char]9)) })
  if (-not $items) { return $null }
  Write-Host ""
  for ($i = 0; $i -lt $items.Count; $i++) {
    $cmd = [string]$items[$i][0]; $actor = [string]$items[$i][2]
    Write-Host (" " + ($i + 1) + " ") -NoNewline -ForegroundColor Black -BackgroundColor DarkCyan
    Write-Host (" " + $cmd) -NoNewline -ForegroundColor Cyan
    $ac = if ($actor -like 'agent:*') { 'DarkYellow' } else { 'DarkGreen' }
    Write-Host ("   " + $actor) -ForegroundColor $ac
  }
  Write-Host ("  reman: pick 1-" + $items.Count + " (Esc)   tip: Install-Module PSReadLine -Force -> arrow/live") -ForegroundColor DarkGray
  $k = [Console]::ReadKey($true); Write-Host ""
  if ([char]::IsDigit($k.KeyChar)) { $num = [int]([string]$k.KeyChar); if ($num -ge 1 -and $num -le $items.Count) { return [string]$items[$num - 1][0] } }
  return $null
}

# Highlight query terms inside a (width-fixed) command string, preserving the row's base colour/bg.
# VT codes are zero-width so visible alignment is unchanged. Empty query -> just the base colour.
function __HiLite {
  param([string]$text, [string]$query, [string]$baseVT, [string]$hiVT)
  if ([string]::IsNullOrWhiteSpace($query)) { return $baseVT + $text }
  $toks = @($query -split '\s+' | Where-Object { $_.Length -ge 2 } | ForEach-Object { [regex]::Escape($_) })
  if (-not $toks) { return $baseVT + $text }
  $rx = [regex]::new(($toks -join '|'), [Text.RegularExpressions.RegexOptions]::IgnoreCase)
  $out = $baseVT; $pos = 0
  foreach ($m in $rx.Matches($text)) {
    if ($m.Index -lt $pos) { continue }
    $out += $text.Substring($pos, $m.Index - $pos) + $hiVT + $m.Value + $baseVT
    $pos = $m.Index + $m.Length
  }
  return $out + $text.Substring($pos)
}

# Full-screen Atuin-style finder (alternate screen, so the prompt is never disturbed). Header on top,
# results below it (best/most-recent first), search bar at the bottom; live semantic filtering as you
# type, Tab toggles all/you/agent. Each frame is built as ONE VT string and written in a single call:
# no ESC[2J blank-then-repaint, so it doesn't flicker; one write instead of ~20 Write-Host = no lag.
function __RemanFind {
  param($initialQuery)
  __RemanEnsure
  # NB: PowerShell variable names are case-insensitive, so these MUST NOT collide with $sel / $bar
  # used below (e.g. a var named $SEL would alias $sel and get clobbered to an int -> type errors).
  $esc = [char]27; $clrRst = "$esc[0m"
  $clrSel = "$esc[97;41m"      # selected row / header: bright white on dark red
  $clrDim = "$esc[37m"         # normal row text: light grey
  $clrHum = "$esc[32m"; $clrAgt = "$esc[33m"; $clrBar = "$esc[91m"   # human=green, agent=yellow, bar=red
  $clrOk = "$esc[92m"; $clrBad = "$esc[91m"; $clrMix = "$esc[93m"    # pass=green, fail=red, mixed=yellow
  [Console]::Write("$esc[?1049h")                 # enter alternate screen buffer
  [Console]::CursorVisible = $false
  $query = $initialQuery; $filter = 'all'; $sel = 0; $statusF = 'all'
  $cwd = (Get-Location).Path; $scopeHere = $true     # default: only commands run in THIS folder
  $refetch = { $sv = if ($scopeHere) { $cwd } else { $null }; @(__RemanFetch $query $filter $sv $statusF) }
  $items = @(& $refetch)
  $descCache = @{}
  $chosen = $null; $done = $false
  try {
    while (-not $done) {
      $H = [Console]::WindowHeight; $W = [Console]::WindowWidth
      $maxShow = $H - 3; if ($maxShow -lt 1) { $maxShow = 1 }   # rows 2..H-2 results; H-1 footer; H bar
      $start = 0
      if ($sel -ge $maxShow) { $start = $sel - $maxShow + 1 }
      $sb = New-Object System.Text.StringBuilder
      # header (VT row 1)
      $scopeLbl = if ($scopeHere) { 'this folder' } else { 'all folders' }
      $hdr = " reman  scope:[" + $scopeLbl + "] actor:[" + $filter + "] pass:[" + $statusF + "]   <>:scope Tab:actor F2:pass . Enter . Esc"
      if ($hdr.Length -gt $W) { $hdr = $hdr.Substring(0, $W) }
      [void]$sb.Append("$esc[1;1H$clrSel" + $hdr.PadRight($W) + $clrRst)
      # results (VT rows 2..H-2), bottom-up: most-recent/best sits just above the footer, older go up
      for ($i = 0; $i -lt $maxShow; $i++) {
        $vrow = ($H - 2) - $i; $idx = $start + $i
        [void]$sb.Append("$esc[$vrow;1H")
        if ($idx -lt $items.Count) {
          $cmd = ([string]$items[$idx][0]) -replace '[\x00-\x1F\x7F]+', ' '   # strip control chars (newlines, BEL=beep, ESC) -> one clean row
          $actor = [string]$items[$idx][1]; $status = [string]$items[$idx][2]
          $mark = if ($idx -eq $sel) { '> ' } else { '  ' }
          $gl = switch ($status) { 'ok' { '+' } 'fail' { 'x' } 'mixed' { '~' } default { ' ' } }   # pass/fail glyph
          $glcol = switch ($status) { 'ok' { $clrOk } 'fail' { $clrBad } 'mixed' { $clrMix } default { $clrDim } }
          $maxc = $W - 17
          if ($cmd.Length -gt $maxc) { $cmd = $cmd.Substring(0, $maxc) }
          $cmd = $cmd.PadRight($maxc)
          $atag = (' ' + $actor); if ($atag.Length -gt 12) { $atag = $atag.Substring(0, 12) }
          $atag = $atag.PadRight(12)
          $acol = if ($actor -like 'agent*') { $clrAgt } else { $clrHum }
          $rowcol = if ($idx -eq $sel) { $clrSel } else { $clrDim }
          $hiVT = if ($idx -eq $sel) { "$esc[93;41m" } else { "$esc[93m" }   # matched query terms: yellow
          # mark + glyph(own colour) + command(row colour, query terms highlighted) + actor
          [void]$sb.Append($mark + $glcol + $gl + ' ' + $clrRst + (__HiLite $cmd $query $rowcol $hiVT) + $clrRst + $acol + $atag + $clrRst)
        } else {
          [void]$sb.Append(' ' * $W)                # clear unused row (no ESC[2J needed)
        }
      }
      # footer (VT row H-1): plain-English description of the selected command (lazy + cached)
      $selCmd = if ($items.Count -gt 0) { [string]$items[$sel][0] } else { '' }
      if ($selCmd -and -not $descCache.ContainsKey($selCmd)) {
        $d = (__RemanSend @{ op = 'describe'; command = $selCmd }).description
        $descCache[$selCmd] = if ($d) { ([string]$d) -replace '[\x00-\x1F\x7F]+', ' ' } else { '' }
      }
      $foot = if ($selCmd -and $descCache[$selCmd]) { '  # ' + $descCache[$selCmd] } else { '' }
      if ($foot.Length -gt $W) { $foot = $foot.Substring(0, $W) }
      [void]$sb.Append("$esc[$($H - 1);1H$clrDim" + $foot.PadRight($W) + $clrRst)
      # search bar (VT row H)
      $bar = "reman> " + $query
      if ($bar.Length -gt $W) { $bar = $bar.Substring($bar.Length - $W) }
      [void]$sb.Append("$esc[$H;1H$clrBar" + $bar.PadRight($W) + $clrRst)
      [Console]::Write($sb.ToString())             # single atomic frame write
      $k = [Console]::ReadKey($true)
      if ($k.Key -eq 'Enter') { if ($items.Count -gt 0) { $chosen = [string]$items[$sel][0] }; $done = $true }
      elseif ($k.Key -eq 'Escape') { $done = $true }
      elseif ($k.Key -eq 'UpArrow') { if ($items.Count) { $sel = [Math]::Min($sel + 1, $items.Count - 1) } }   # up = older
      elseif ($k.Key -eq 'DownArrow') { if ($items.Count) { $sel = [Math]::Max($sel - 1, 0) } }                 # down = more recent
      elseif ($k.Key -eq 'LeftArrow' -or $k.Key -eq 'RightArrow') { $scopeHere = -not $scopeHere; $sel = 0; $items = @(& $refetch) }   # toggle this-folder <-> all
      elseif ($k.Key -eq 'F2') { $statusF = switch ($statusF) { 'all' { 'ok' } 'ok' { 'fail' } default { 'all' } }; $sel = 0; $items = @(& $refetch) }   # pass/fail filter
      elseif ($k.Key -eq 'Tab') { $filter = switch ($filter) { 'all' { 'you' } 'you' { 'agent' } default { 'all' } }; $sel = 0; $items = @(& $refetch) }
      elseif ($k.Key -eq 'Backspace') { if ($query.Length -gt 0) { $query = $query.Substring(0, $query.Length - 1); $sel = 0; $items = @(& $refetch) } }
      elseif ($k.KeyChar -and -not [char]::IsControl($k.KeyChar)) { $query += $k.KeyChar; $sel = 0; $items = @(& $refetch) }
    }
  } finally {
    [Console]::CursorVisible = $true
    [Console]::Write("$esc[?1049l")                 # restore the main screen (prompt intact)
  }
  return $chosen
}

if (Get-Module PSReadLine -ErrorAction SilentlyContinue) {
  # Ctrl+R: quick semantic search of what's already typed (static arrow-select).
  Set-PSReadLineKeyHandler -Chord 'Ctrl+r' -BriefDescription 'Reman semantic recall' -ScriptBlock {
    $line = $null; $cur = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cur)
    if ([string]::IsNullOrWhiteSpace($line)) { return }
    $v = (Get-Module PSReadLine).Version
    if (($v.Major -gt 2) -or ($v.Major -eq 2 -and $v.Minor -ge 1)) {
      $res = __RemanPick $line $false
      if ($res.chosen) { [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $res.chosen) }
      [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt($null, $res.promptY)
    } else {
      $chosen = __RemanNumbered $line
      if ($chosen) { [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $chosen) }
      [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    }
  }

  # UpArrow: full-screen Atuin-style finder (history + type-to-filter + Tab toggles all/you/agent).
  Set-PSReadLineKeyHandler -Chord 'UpArrow' -BriefDescription 'Reman history finder' -ScriptBlock {
    $line = $null; $cur = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cur)
    if ($line.Contains("`n")) { [Microsoft.PowerShell.PSConsoleReadLine]::PreviousLine(); return }
    $v = (Get-Module PSReadLine).Version
    if (($v.Major -gt 2) -or ($v.Major -eq 2 -and $v.Minor -ge 1)) {
      $chosen = __RemanFind $line          # full-screen finder (alt screen, prompt untouched)
      if ($chosen) { [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $chosen) }
      [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    } else {
      $chosen = __RemanNumbered $line
      if ($chosen) { [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $chosen) }
      [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    }
  }
}

# Fire-and-forget push to the daemon (no wait for a reply) - keeps the prompt instant.
function __RemanPush {
  param($obj)
  try {
    $c = New-Object Net.Sockets.TcpClient; $c.Connect('127.0.0.1', 8765)
    $st = $c.GetStream()
    $b = [Text.Encoding]::UTF8.GetBytes(($obj | ConvertTo-Json -Compress) + "`n")
    $st.Write($b, 0, $b.Length); $st.Flush(); $c.Close()
  } catch { }
}

# Live exit-code capture (go-forward pass/fail). Atuin records exit=-1 (unknown) on PowerShell, so
# reman cannot tell pass from fail from the sync alone. Here we grab the REAL result of each command
# you run ($? / $LASTEXITCODE) and push it, so pass/fail filtering grows as you work. Wraps the
# existing prompt (guarded against double-wrap), preserves $LASTEXITCODE, and never blocks.
# To disable: Remove-Item Function:prompt; $global:__RemanPromptHooked=$false; then reload the profile.
if (-not $global:__RemanPromptHooked) {
  $global:__RemanPromptHooked = $true
  $global:__RemanOrigPrompt = $function:prompt
  $global:__RemanLastHistId = -1
  function global:prompt {
    $code = $LASTEXITCODE; $ok = $?               # MUST be first: reflect the just-run command
    try {
      $h = Get-History -Count 1
      if ($h -and $h.Id -ne $global:__RemanLastHistId) {
        $global:__RemanLastHistId = $h.Id
        $cmd = $h.CommandLine
        if ($cmd -and $cmd.Trim().Length -gt 1) {
          $exit = if ($ok) { 0 } elseif ($code) { $code } else { 1 }
          __RemanPush @{ op = 'ingest'; command = $cmd; exit = $exit; cwd = (Get-Location).Path; actor = 'human' }
        }
      }
    } catch { }
    $global:LASTEXITCODE = $code                  # restore for the original prompt's exit indicator
    if ($global:__RemanOrigPrompt) { & $global:__RemanOrigPrompt }
    else { "PS " + (Get-Location).Path + "> " }
  }
}
'''

# zsh / bash: fzf-based live finder (history default, type to semantic-search). Bound to UpArrow
# and Ctrl+R. fzf respects --height on macOS/Linux, so it renders inline below the prompt.
ZSH = r'''
reman() { "__PY__" "__DAEMON__" "$@" }
_reman_finder() {
  local picked
  picked=$( "__PY__" "__DAEMON__" recentfull 2>/dev/null | fzf \
    --layout default --delimiter $'\t' --with-nth 1,3 --no-sort \
    --info inline --border rounded --prompt 'reman> ' --pointer '>' \
    --disabled --query "$BUFFER" \
    --bind 'change:reload([ -z {q} ] && "__PY__" "__DAEMON__" recentfull || "__PY__" "__DAEMON__" completefull {q})' \
    --header 'reman: history / type to semantic-search' )
  if [ -n "$picked" ]; then
    BUFFER="${picked%%$'\t'*}"
    CURSOR=${#BUFFER}
  fi
  zle reset-prompt
}
zle -N _reman_finder
bindkey '^R' _reman_finder
bindkey "$terminfo[kcuu1]" _reman_finder 2>/dev/null
bindkey '^[[A' _reman_finder
'''

BASH = r'''
reman() { "__PY__" "__DAEMON__" "$@"; }
_reman_finder() {
  local picked
  picked=$( "__PY__" "__DAEMON__" recentfull 2>/dev/null | fzf \
    --layout default --delimiter $'\t' --with-nth 1,3 --no-sort \
    --info inline --border rounded --prompt 'reman> ' --pointer '>' \
    --disabled --query "$READLINE_LINE" \
    --bind 'change:reload([ -z {q} ] && "__PY__" "__DAEMON__" recentfull || "__PY__" "__DAEMON__" completefull {q})' \
    --header 'reman: history / type to semantic-search' )
  if [ -n "$picked" ]; then
    READLINE_LINE="${picked%%$'\t'*}"
    READLINE_POINT=${#READLINE_LINE}
  fi
}
bind -x '"\C-r": _reman_finder'
bind -x '"\e[A": _reman_finder'
'''

TEMPLATES = {"powershell": POWERSHELL, "zsh": ZSH, "bash": BASH}


def main():
    shell = sys.argv[1].lower() if len(sys.argv) > 1 else ""
    tpl = TEMPLATES.get(shell)
    if not tpl:
        sys.stderr.write("usage: reman_init.py [powershell|zsh|bash]\n")
        sys.exit(1)
    sys.stdout.write(tpl.replace("__PY__", PY).replace("__DAEMON__", DAEMON))


if __name__ == "__main__":
    main()
