# Installs corvo-chat (https://github.com/Abinsanth/corvo-chat-client) on Windows:
#
#   irm https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.ps1 | iex
#
# Installs the latest release to %LOCALAPPDATA%\Programs\corvo-chat, or to
# $env:CORVO_CHAT_INSTALL_DIR if set, and adds it to your user PATH unless
# $env:CORVO_CHAT_NO_MODIFY_PATH is set. Set
# $env:CORVO_CHAT_VERSION (like v0.1.0-rc.1) for a particular release,
# pre-releases included. Never needs admin.

# Everything runs from here, so a download cut short by `irm | iex` runs nothing.
function Install-CorvoChat {
    $ErrorActionPreference = 'Stop'
    # The progress bar makes Invoke-WebRequest crawl on Windows PowerShell.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2, which GitHub requires.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'Abinsanth/corvo-chat-client'
    $app = 'corvo-chat'
    $installDir = if ($env:CORVO_CHAT_INSTALL_DIR) { $env:CORVO_CHAT_INSTALL_DIR }
        else { Join-Path $env:LOCALAPPDATA "Programs\$app" }
    $releaseUrl = if ($env:CORVO_CHAT_VERSION) { "https://github.com/$repo/releases/download/$env:CORVO_CHAT_VERSION" }
        else { "https://github.com/$repo/releases/latest/download" }
    # Overridable only to test this script against a local copy of a release.
    $baseUrl = if ($env:CORVO_CHAT_BASE_URL) { $env:CORVO_CHAT_BASE_URL } else { $releaseUrl }

    # ARM64 Windows runs the x64 build through emulation.
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        throw "$app install: no build for a $arch CPU; build from source: https://github.com/$repo"
    }

    $asset = "$app-x86_64-pc-windows-msvc.zip"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) "$app-install-$PID"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        Write-Output "downloading $asset"
        $zip = Join-Path $tmp $asset
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset" -OutFile $zip
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset.sha256" -OutFile "$zip.sha256"
        } catch {
            throw "$app install: couldn't download $baseUrl/$asset ($($_.Exception.Message))"
        }
        $expected = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash
        if ($expected -ne $actual) {
            throw "$app install: checksum mismatch for $asset; not installing"
        }

        $out = Join-Path $tmp 'out'
        Expand-Archive -Path $zip -DestinationPath $out
        $new = Join-Path $out "$app.exe"
        if (-not (Test-Path $new)) { throw "$app install: the download didn't contain $app.exe" }

        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        $exe = Join-Path $installDir "$app.exe"
        $old = Join-Path $installDir "$app.old.exe"
        # A running exe can't be overwritten or deleted, but it can be renamed.
        Remove-Item -Force -ErrorAction SilentlyContinue $old
        $movedAside = $false
        if (Test-Path $exe) {
            try { Move-Item $exe $old; $movedAside = $true } catch { $movedAside = $false }
        }
        # Copy beside the target and rename, so a failure never leaves half an exe.
        try {
            Copy-Item -Force $new "$exe.new"
            Move-Item -Force "$exe.new" $exe
        } catch {
            Remove-Item -Force -ErrorAction SilentlyContinue "$exe.new"
            if ($movedAside) {
                try { Move-Item -Force $old $exe } catch {
                    throw "$app install: couldn't write $exe, and the old version is at $old; rename it back to $app.exe"
                }
            }
            throw "$app install: couldn't write $exe; close $app and try again"
        }
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
    }

    Write-Output "installed $(& $exe --version) to $installDir"
    # Read the raw value: the expanded one would freeze %VARS% and turn the
    # value into a plain string when written back.
    $key = Get-Item 'HKCU:\Environment'
    $userPath = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    # Compare the way Windows reads an entry: %VARS% expanded, quotes and a
    # trailing backslash dropped (-contains ignores case), so a folder that's
    # already there in another spelling isn't added a second time.
    $clean = { param($e) [Environment]::ExpandEnvironmentVariables($e.Trim().Trim('"')).TrimEnd('\') }
    $entries = @($userPath -split ';' | Where-Object { $_.Trim() } | ForEach-Object { & $clean $_ })
    if ($entries -contains (& $clean $installDir)) {
        Write-Output "run: $app"
    } elseif ($env:CORVO_CHAT_NO_MODIFY_PATH) {
        Write-Output ''
        Write-Output "$installDir isn't on your PATH. Add it to your user PATH (search Windows for"
        Write-Output "'Edit environment variables for your account'), then open a new terminal."
    } else {
        $newPath = if ($userPath.Trim()) { $userPath.TrimEnd(';') + ";$installDir" } else { $installDir }
        Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value $newPath -Type ExpandString
        # Written down so `corvo-chat --uninstall` removes this entry, and only
        # because the installer added it (never one that was already there).
        # A plain string (REG_SZ): that is all uninstall reads.
        New-Item -Force -Path "HKCU:\Software\$app" | Out-Null
        Set-ItemProperty -Path "HKCU:\Software\$app" -Name PathAdded -Value $installDir -Type String
        # Set-ItemProperty doesn't tell Explorer the environment changed; these calls do, so new terminals see PATH.
        [Environment]::SetEnvironmentVariable('CORVO_CHAT_INSTALLING', '1', 'User')
        [Environment]::SetEnvironmentVariable('CORVO_CHAT_INSTALLING', $null, 'User')
        $env:Path = "$env:Path;$installDir"
        Write-Output ''
        Write-Output "added $installDir to your PATH. Open a new terminal, then run: $app"
    }
}

Install-CorvoChat
