# Install the cgr binary from a GitHub release (Windows).
#
#   irm https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.ps1 | iex
#
# Environment: CGR_VERSION (default: latest release), CGR_INSTALL_DIR (default: %LOCALAPPDATA%\Programs\cgr),
#              CGR_BASE_URL (default: the GitHub releases of this repository)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'NianJiuZst/CodeGraph-Rust'
$base = if ($env:CGR_BASE_URL) { $env:CGR_BASE_URL } else { "https://github.com/$repo/releases/download" }
$dest = if ($env:CGR_INSTALL_DIR) { $env:CGR_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cgr' }

$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"

$version = $env:CGR_VERSION
if (-not $version) {
    $resp = Invoke-WebRequest -Uri "https://github.com/$repo/releases/latest" -MaximumRedirection 0 -ErrorAction SilentlyContinue -SkipHttpErrorCheck
    $loc = $resp.Headers.Location
    if (-not $loc) { throw 'no release has been published yet' }
    $version = ([string]$loc).Split('/')[-1]
}
$version = $version.TrimStart('v')

$name = "cgr-$version-$target"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("cgr-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $name"
    Invoke-WebRequest -Uri "$base/v$version/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest -Uri "$base/v$version/$name.zip.sha256" -OutFile "$tmp\$name.zip.sha256"
    $expected = ((Get-Content "$tmp\$name.zip.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch (expected $expected, got $actual)" }

    Expand-Archive -Path "$tmp\$name.zip" -DestinationPath $tmp
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item "$tmp\$name\cgr.exe" (Join-Path $dest 'cgr.exe') -Force
    Write-Host "installed $dest\cgr.exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dest) {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$dest", 'User')
        Write-Host "added $dest to your PATH (open a new terminal)"
    }
    if ($env:CGR_INSTALL_SKILL -eq '1') {
        $skillScriptUrl = if ($env:CGR_SKILL_SCRIPT_URL) { $env:CGR_SKILL_SCRIPT_URL } else { "https://raw.githubusercontent.com/$repo/main/scripts/install-skill.ps1" }
        Invoke-Expression (Invoke-WebRequest -Uri $skillScriptUrl -UseBasicParsing).Content
    }
    Write-Host 'next:  cd your-project; cgr init      # build the index'
    Write-Host "       irm https://raw.githubusercontent.com/$repo/main/scripts/install-skill.ps1 | iex  # install SKILL.md"
    Write-Host '       cgr install                    # register MCP with Claude Code, Cursor and Codex'
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
