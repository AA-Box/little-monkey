Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName WindowsBase

$profilePath = Join-Path ([System.IO.Path]::GetTempPath()) 'little-monkey-testapp-profile.json'
$window = New-Object System.Windows.Window
$window.Title = 'Little Monkey TestApp'
$window.Width = 640
$window.Height = 420
$window.WindowStartupLocation = 'CenterScreen'
$window.Name = 'LittleMonkeyTestApp'

$grid = New-Object System.Windows.Controls.Grid
$grid.Margin = New-Object System.Windows.Thickness(16)
foreach ($height in @(46, 34, 34, 42, 42, 42, 42, 42, 42)) {
    $row = New-Object System.Windows.Controls.RowDefinition
    $row.Height = New-Object System.Windows.GridLength($height)
    $grid.RowDefinitions.Add($row)
}

function Add-TextBlock([string]$text, [int]$row, [string]$automationName = '') {
    $block = New-Object System.Windows.Controls.TextBlock
    $block.Text = $text
    $block.VerticalAlignment = [System.Windows.VerticalAlignment]::Center
    if ($automationName) {
        [System.Windows.Automation.AutomationProperties]::SetName($block, $automationName)
    }
    [System.Windows.Controls.Grid]::SetRow($block, $row)
    $grid.Children.Add($block) | Out-Null
    return $block
}

$title = Add-TextBlock 'Little Monkey TestApp' 0
$title.FontSize = 24
$title.FontWeight = [System.Windows.FontWeights]::Bold

$profilePanel = New-Object System.Windows.Controls.StackPanel
$profilePanel.Orientation = [System.Windows.Controls.Orientation]::Horizontal
[System.Windows.Controls.Grid]::SetRow($profilePanel, 1)
$grid.Children.Add($profilePanel) | Out-Null
$profileLabel = New-Object System.Windows.Controls.TextBlock
$profileLabel.Text = 'Profile name'
$profileLabel.Width = 104
$profileLabel.VerticalAlignment = [System.Windows.VerticalAlignment]::Center
$profilePanel.Children.Add($profileLabel) | Out-Null
$profile = New-Object System.Windows.Controls.TextBox
$profile.Name = 'ProfileInput'
$profile.Width = 300
$profile.VerticalContentAlignment = [System.Windows.VerticalAlignment]::Center
$profile.Text = 'Test profile'
[System.Windows.Automation.AutomationProperties]::SetName($profile, 'Profile name')
[System.Windows.Automation.AutomationProperties]::SetAutomationId($profile, 'ProfileInput')
$profilePanel.Children.Add($profile) | Out-Null
$save = New-Object System.Windows.Controls.Button
$save.Name = 'SaveProfile'
$save.Content = 'Save profile'
$save.Width = 150
$save.Margin = New-Object System.Windows.Thickness(16, 0, 0, 0)
[System.Windows.Automation.AutomationProperties]::SetName($save, 'Save profile')
$profilePanel.Children.Add($save) | Out-Null

$status = Add-TextBlock 'Not saved' 2 'Save status'

$dark = New-Object System.Windows.Controls.CheckBox
$dark.Name = 'DarkMode'
$dark.Content = 'Dark mode'
$dark.Margin = New-Object System.Windows.Thickness(0, 4, 0, 4)
[System.Windows.Automation.AutomationProperties]::SetName($dark, 'Dark mode')
[System.Windows.Automation.AutomationProperties]::SetAutomationId($dark, 'DarkMode')
[System.Windows.Automation.AutomationProperties]::SetHelpText($dark, 'Off')
[System.Windows.Controls.Grid]::SetRow($dark, 3)
$grid.Children.Add($dark) | Out-Null
$dark.Add_Checked({
    [System.Windows.Automation.AutomationProperties]::SetHelpText($dark, 'On')
})
$dark.Add_Unchecked({
    [System.Windows.Automation.AutomationProperties]::SetHelpText($dark, 'Off')
})

$disabled = New-Object System.Windows.Controls.Button
$disabled.Content = 'Disabled button'
$disabled.IsEnabled = $false
[System.Windows.Automation.AutomationProperties]::SetName($disabled, 'Disabled button')
[System.Windows.Controls.Grid]::SetRow($disabled, 4)
$grid.Children.Add($disabled) | Out-Null

$dynamic = New-Object System.Windows.Controls.Button
$dynamic.Content = 'Add dynamic item'
[System.Windows.Automation.AutomationProperties]::SetName($dynamic, 'Add dynamic item')
[System.Windows.Controls.Grid]::SetRow($dynamic, 5)
$grid.Children.Add($dynamic) | Out-Null
$dynamic.Add_Click({
    $item = Add-TextBlock 'Dynamic item' 8
    [System.Windows.Automation.AutomationProperties]::SetName($item, 'Dynamic item')
})

$destructive = New-Object System.Windows.Controls.Button
$destructive.Content = 'Destructive action'
[System.Windows.Automation.AutomationProperties]::SetName($destructive, 'Destructive action')
[System.Windows.Controls.Grid]::SetRow($destructive, 6)
$grid.Children.Add($destructive) | Out-Null

$passwordLabel = Add-TextBlock 'Fake password field (must be blocked)' 7
$password = New-Object System.Windows.Controls.PasswordBox
$password.Name = 'FakePassword'
$password.Width = 400
[System.Windows.Automation.AutomationProperties]::SetName($password, 'Fake password field (must be blocked)')
[System.Windows.Controls.Grid]::SetRow($password, 8)
$grid.Children.Add($password) | Out-Null

if (Test-Path -LiteralPath $profilePath) {
    try {
        $saved = Get-Content -Raw -LiteralPath $profilePath -ErrorAction Stop | ConvertFrom-Json
        if ($saved.profile -is [string]) { $profile.Text = $saved.profile }
        if ($saved.dark -eq $true) { $dark.IsChecked = $true }
    } catch {}
}

$save.Add_Click({
    @{profile=$profile.Text;dark=[bool]$dark.IsChecked} | ConvertTo-Json | Set-Content -LiteralPath $profilePath -Encoding UTF8
    $status.Text = 'Saved'
    [System.Windows.Automation.AutomationProperties]::SetName($status, 'Saved')
})

$window.Content = $grid
$secondary = New-Object System.Windows.Window
$secondary.Title = 'Little Monkey TestApp Secondary'
$secondary.Width = 320
$secondary.Height = 180
$secondary.WindowStartupLocation = 'Manual'
$secondary.Left = 700
$secondary.Top = 120
$secondaryLabel = New-Object System.Windows.Controls.TextBlock
$secondaryLabel.Text = 'Little Monkey TestApp Secondary'
$secondaryLabel.HorizontalAlignment = [System.Windows.HorizontalAlignment]::Center
$secondaryLabel.VerticalAlignment = [System.Windows.VerticalAlignment]::Center
[System.Windows.Automation.AutomationProperties]::SetName($secondaryLabel, 'Little Monkey TestApp Secondary')
$secondary.Content = $secondaryLabel

$secondary.Show()
$window.Show()
[System.Windows.Threading.Dispatcher]::Run()
