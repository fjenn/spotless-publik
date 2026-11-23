# The YouTube sign-in window on Windows, opened and closed the way a user does, with no WebDriver
# or DevTools port anywhere near the app: Settings › Sign in, then the window's own ✕. Afterwards
# the app must be whole: still running, the main page still answering UI Automation, and no
# WebView2 errors in its log.
param([Parameter(Mandatory)] [string] $Exe, [string] $Out = "$env:RUNNER_TEMP\e2e-out")
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
New-Item -ItemType Directory -Force "$Out\shots" | Out-Null
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
function Shot($n) { $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height; [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size); $bmp.Save("$Out\shots\$n.png") }
function Win($name) { $A::RootElement.FindFirst($Scope::Children, (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $name))) }
function Buttons($w) { $w.FindAll($Scope::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button))) }
function PressAny($w, $name) { $e = $w.FindFirst($Scope::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $name))); if ($e) { try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); return $true } catch { try { $e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern).Select(); return $true } catch {} } }; return $false }
function Press($w, $name) { foreach ($b in Buttons $w) { if ($b.Current.Name -eq $name) { $b.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); return $true } }; return $false }

$p = Start-Process $Exe -PassThru
$main = $null
for ($i = 0; $i -lt 40 -and -not $main; $i++) { Start-Sleep 2; $main = Win 'Spotless' }
if (-not $main) { throw 'main window never appeared' }
Start-Sleep 8
for ($i = 0; $i -lt 3; $i++) { Press $main 'Not now' | Out-Null; Start-Sleep 1 }
[System.Windows.Forms.SendKeys]::SendWait('^,')   # Settings
Start-Sleep 4
$rounds = 0
for ($round = 1; $round -le 3; $round++) {
  # Each round finds Settings' "Sign in" anew: the main page has to be alive and answering.
  $main = Win 'Spotless'
  $ok = $false
  for ($i = 0; $i -lt 15 -and -not $ok; $i++) { $ok = Press $main 'Sign in'; if (-not $ok) { Start-Sleep 1 } }
  if (-not $ok -and $round -eq 1) {
    Press $main 'Account and settings' | Out-Null; Start-Sleep 1
    PressAny $A::RootElement 'Settings' | Out-Null; Start-Sleep 3
    for ($i = 0; $i -lt 10 -and -not $ok; $i++) { $ok = Press $main 'Sign in'; if (-not $ok) { Start-Sleep 1 } }
  }
  if (-not $ok) { Shot "signin-no-button-$round"; break }
  $login = $null
  for ($i = 0; $i -lt 30 -and -not $login; $i++) { Start-Sleep 1; $login = $A::RootElement.FindFirst($Scope::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, 'Sign in to YouTube Music · Spotless'))) }
  if (-not $login) { Shot "signin-no-window-$round"; break }
  Start-Sleep 5
  Shot "signin-open-$round"
  $login.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close()   # the window's ✕
  Start-Sleep 6
  # The sheet behind it may still be up: its Close/Cancel.
  # (Its "Cancel", not "Close": the title bar's own ✕ is also a button named Close, and that one
  # hides the app to the tray, which is what pressed it in an earlier version of this probe.)
  Press (Win 'Spotless') 'Cancel' | Out-Null
  Start-Sleep 3
  Shot "signin-closed-$round"
  $state = if ($p.HasExited) { "EXITED code $($p.ExitCode)" } elseif (-not (Win 'Spotless')) { 'running, main window not on screen' } else { 'running, window shown' }
  Write-Host "round ${round} after closing the sign-in window: $state"
  Copy-Item "$env:APPDATA\app.spotless.desktop\encore.log" "$Out\signin-encore-$round.log" -ErrorAction SilentlyContinue
  if ($p.HasExited -or -not (Win 'Spotless')) { break }
  $rounds = $round
}
$alive = -not $p.HasExited
$main = Win 'Spotless'
$answering = $rounds -eq 3
$log = Get-Content "$env:APPDATA\app.spotless.desktop\encore.log" -Raw
$after = $log.Substring([Math]::Max(0, $log.IndexOf('sign-in window finished')))
$errors = ($after -split "`n" | Where-Object { $_ -match 'WebView2 error|ERROR tauri_runtime_wry' }).Count
Write-Host "rounds: $rounds/3  alive: $alive  main page answering: $answering  WebView2 errors after closing: $errors"
"### Sign-in window closed by the user: alive $alive, main page answering $answering, WebView2 errors $errors" | Out-File -Append $env:GITHUB_STEP_SUMMARY
Copy-Item "$env:APPDATA\app.spotless.desktop\encore.log" "$Out\signin-encore.log"
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
if (-not $alive -or -not $answering -or $errors -gt 0) { throw 'the app did not survive the sign-in window closing' }
