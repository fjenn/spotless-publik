# Windows half of the updater E2E, without WebDriver: start the installed (older) app, wait for its
# own startup check to show the "update available" banner, press Install through UI Automation
# (WebView2 exposes the page to UIA like any window), then confirm the newer version is installed
# and running again. Exit code 0 = updated and relaunched.
param([Parameter(Mandatory)] [string] $Exe, [Parameter(Mandatory)] [string] $Want, [string] $Shots = "$env:RUNNER_TEMP\e2e-out\shots")
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
New-Item -ItemType Directory -Force $Shots | Out-Null
function Shot($name) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
  $bmp.Save("$Shots\$name.png")
}
function Version { (Get-ChildItem HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall | Get-ItemProperty | Where-Object DisplayName -match 'Spotless').DisplayVersion }

Write-Host "installed before: $(Version)"
Start-Process $Exe | Out-Null
$A = [System.Windows.Automation.AutomationElement]
$button = New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button)
$install = $null
for ($i = 0; $i -lt 60 -and -not $install; $i++) {
  Start-Sleep 3
  $win = $A::RootElement.FindFirst([System.Windows.Automation.TreeScope]::Children, (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, 'Spotless')))
  if ($win) {
    # Dismiss first-run dialogs ("Not now") so the banner is reachable.
    foreach ($b in $win.FindAll([System.Windows.Automation.TreeScope]::Descendants, $button)) {
      if ($b.Current.Name -eq 'Not now') { try { $b.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() } catch {} }
      if ($b.Current.Name -eq 'Install') { $install = $b }
    }
  }
}
Shot 'update-banner'
if (-not $install) { throw "no Install button (update banner) within 3 minutes" }
$install.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
Write-Host "Install pressed"
$v = $null; $p = $null
for ($i = 0; $i -lt 60; $i++) {
  Start-Sleep 5
  $v = Version; $p = Get-Process Spotless -ErrorAction SilentlyContinue
  if ($v -eq $Want -and $p) { break }
}
Start-Sleep 10
Shot 'after-update'
Write-Host "installed after: $v; running: $([bool]$p)"
"### Windows update → installed $v, running $([bool]$p)" | Out-File -Append $env:GITHUB_STEP_SUMMARY
if ($v -ne $Want) { throw "installed version is $v, wanted $Want" }
if (-not $p) { throw "Spotless did not relaunch after the update" }
