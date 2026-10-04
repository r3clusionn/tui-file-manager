# Runs fm in a real console window and types into it with SendKeys, then checks the result on disk.
# This covers what the --keys tests cannot: crossterm's raw mode, key events and the draw loop.
#   powershell -File scripts/console_test.ps1
$ErrorActionPreference = 'Stop'
$fm = Join-Path $PSScriptRoot '..\target\release\fm.exe'
$dir = Join-Path ([IO.Path]::GetTempPath()) ("fm-console-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path (Join-Path $dir 'sub') | Out-Null
Set-Content -Path (Join-Path $dir 'sub\a.txt') -Value 'alpha'
$cdFile = Join-Path $dir 'cd.txt'

$p = Start-Process -FilePath $fm -ArgumentList @("`"$dir`"", '--no-config', '--cd-file', "`"$cdFile`"") -PassThru
Start-Sleep -Milliseconds 1500
# A window started from the foreground process gets the focus, so the keys go to fm.
if ($p.HasExited) { Remove-Item -Recurse -Force $dir; throw "fm exited early with code $($p.ExitCode)" }
Add-Type -AssemblyName System.Windows.Forms
function Send($k) { [System.Windows.Forms.SendKeys]::SendWait($k); Start-Sleep -Milliseconds 250 }
# New file, then into sub, copy a.txt, back up, paste, rename the copy, quit.
Send 'a'; Send 'made-by-keys.txt'; Send '{ENTER}'
Send 'g'; Send 'l'; Send 'y'; Send 'h'; Send 'p'
Start-Sleep -Milliseconds 500
Send '/'; Send 'a.txt'; Send '{ENTER}'; Send 'r'; Send '{END}'; Send '^u'; Send 'copied.txt'; Send '{ENTER}'
Send '{ESC}'; Send 'g'; Send 'l'; Send 'q'
$p.WaitForExit(5000) | Out-Null
$ok = $p.HasExited -and $p.ExitCode -eq 0
$made = Test-Path (Join-Path $dir 'made-by-keys.txt')
$copied = (Test-Path (Join-Path $dir 'copied.txt')) -and ((Get-Content (Join-Path $dir 'copied.txt')) -eq 'alpha')
$orig = Test-Path (Join-Path $dir 'sub\a.txt')
$cd = if (Test-Path $cdFile) { Get-Content $cdFile } else { '' }
$cdOk = $cd -eq (Join-Path $dir 'sub')
"exited 0: $ok; created: $made; copied and renamed: $copied; original kept: $orig; cd file: $cdOk ($cd)"
if (-not $p.HasExited) { Stop-Process -Id $p.Id }
Remove-Item -Recurse -Force $dir
if (-not ($ok -and $made -and $copied -and $orig -and $cdOk)) { exit 1 }
