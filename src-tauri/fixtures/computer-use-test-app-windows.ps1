Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$profilePath = Join-Path ([System.IO.Path]::GetTempPath()) 'little-monkey-testapp-profile.json'
$form = New-Object System.Windows.Forms.Form
$form.Text = 'Little Monkey TestApp'
$form.ClientSize = New-Object System.Drawing.Size(640, 420)
$form.StartPosition = 'CenterScreen'

$title = New-Object System.Windows.Forms.Label
$title.Text = 'Little Monkey TestApp'
$title.Font = New-Object System.Drawing.Font('Segoe UI', 18, [System.Drawing.FontStyle]::Bold)
$title.Location = New-Object System.Drawing.Point(16, 14)
$title.AutoSize = $true

$profileLabel = New-Object System.Windows.Forms.Label
$profileLabel.Text = 'Profile name'
$profileLabel.Location = New-Object System.Drawing.Point(16, 70)
$profileLabel.AutoSize = $true

$profile = New-Object System.Windows.Forms.TextBox
$profile.AccessibleName = 'Profile name'
$profile.Location = New-Object System.Drawing.Point(120, 66)
$profile.Size = New-Object System.Drawing.Size(300, 24)
$profile.Text = 'Test profile'

$save = New-Object System.Windows.Forms.Button
$save.Text = 'Save profile'
$save.Location = New-Object System.Drawing.Point(440, 64)
$save.Size = New-Object System.Drawing.Size(150, 30)

$status = New-Object System.Windows.Forms.Label
$status.Text = 'Not saved'
$status.AccessibleName = 'Save status'
$status.Location = New-Object System.Drawing.Point(16, 104)
$status.AutoSize = $true

$dark = New-Object System.Windows.Forms.CheckBox
$dark.Text = 'Dark mode'
$dark.AccessibleName = 'Dark mode'
$dark.Location = New-Object System.Drawing.Point(16, 148)
$dark.AutoSize = $true

$disabled = New-Object System.Windows.Forms.Button
$disabled.Text = 'Disabled button'
$disabled.Location = New-Object System.Drawing.Point(150, 144)
$disabled.Size = New-Object System.Drawing.Size(150, 30)
$disabled.Enabled = $false

$dynamic = New-Object System.Windows.Forms.Button
$dynamic.Text = 'Add dynamic item'
$dynamic.Location = New-Object System.Drawing.Point(320, 144)
$dynamic.Size = New-Object System.Drawing.Size(150, 30)
$dynamic.Add_Click({
    $item = New-Object System.Windows.Forms.Label
    $item.Text = 'Dynamic item'
    $item.Location = New-Object System.Drawing.Point(16, 340)
    $item.AutoSize = $true
    $form.Controls.Add($item)
})

$destructive = New-Object System.Windows.Forms.Button
$destructive.Text = 'Destructive action'
$destructive.Location = New-Object System.Drawing.Point(480, 144)
$destructive.Size = New-Object System.Drawing.Size(140, 30)

$passwordLabel = New-Object System.Windows.Forms.Label
$passwordLabel.Text = 'Fake password field (must be blocked)'
$passwordLabel.Location = New-Object System.Drawing.Point(16, 210)
$passwordLabel.AutoSize = $true

$password = New-Object System.Windows.Forms.TextBox
$password.AccessibleName = 'Fake password field (must be blocked)'
$password.UseSystemPasswordChar = $true
$password.Location = New-Object System.Drawing.Point(16, 238)
$password.Size = New-Object System.Drawing.Size(400, 24)

if (Test-Path -LiteralPath $profilePath) {
    try {
        $saved = Get-Content -Raw -LiteralPath $profilePath -ErrorAction Stop | ConvertFrom-Json
        if ($saved.profile -is [string]) { $profile.Text = $saved.profile }
        if ($saved.dark -eq $true) { $dark.Checked = $true }
    } catch {}
}

$save.Add_Click({
    @{profile=$profile.Text;dark=$dark.Checked} | ConvertTo-Json | Set-Content -LiteralPath $profilePath -Encoding UTF8
    $status.Text = 'Saved'
})

$form.Controls.AddRange(@($title, $profileLabel, $profile, $save, $status, $dark, $disabled, $dynamic, $destructive, $passwordLabel, $password))
[System.Windows.Forms.Application]::Run($form)
