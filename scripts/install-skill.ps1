# Install the official codegraph-rust Agent Skill (SKILL.md) on Windows.
#
#   irm https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install-skill.ps1 | iex
#
# Environment:
#   CGR_SKILL_REF      Git branch or tag to fetch from (default: main)
#   CGR_SKILL_URL      Override download URL for SKILL.md
#   CGR_SKILL_SOURCE   Local SKILL.md path (skips network download)
#   CGR_SKILL_TARGETS  Comma-separated targets: claude,codex,antigravity,cursor,opencode,all
#   CGR_SKILL_DIR      Custom destination directory (<dir>\codegraph-rust\SKILL.md)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'NianJiuZst/CodeGraph-Rust'
$ref = if ($env:CGR_SKILL_REF) { $env:CGR_SKILL_REF } else { 'main' }
$url = if ($env:CGR_SKILL_URL) { $env:CGR_SKILL_URL } else { "https://raw.githubusercontent.com/$repo/$ref/skills/codegraph-rust/SKILL.md" }

$roots = [ordered]@{
    claude      = if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $HOME '.claude' }
    codex       = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
    antigravity = if ($env:GEMINI_CONFIG_DIR) { $env:GEMINI_CONFIG_DIR } else { Join-Path $HOME '.gemini\config' }
    cursor      = Join-Path $HOME '.cursor'
    opencode    = if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME 'opencode' } else { Join-Path $HOME '.config\opencode' }
}
$labels = @{
    claude      = 'Claude Code'
    codex       = 'Codex'
    antigravity = 'Antigravity'
    cursor      = 'Cursor'
    opencode    = 'OpenCode'
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("cgr-skill-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $src = Join-Path $tmp 'SKILL.md'
    if ($env:CGR_SKILL_SOURCE) {
        if (-not (Test-Path $env:CGR_SKILL_SOURCE)) { throw "local skill source not found: $env:CGR_SKILL_SOURCE" }
        Copy-Item $env:CGR_SKILL_SOURCE $src -Force
    } else {
        Write-Host "downloading codegraph-rust SKILL.md ($ref)"
        Invoke-WebRequest -Uri $url -OutFile $src
    }
    $content = Get-Content $src -Raw
    if ($content -notmatch '(?m)^name:\s*codegraph-rust\s*$') {
        throw 'downloaded file is not a valid codegraph-rust SKILL.md'
    }

    function Install-SkillTo([string]$label, [string]$baseDir) {
        $destDir = Join-Path $baseDir 'codegraph-rust'
        New-Item -ItemType Directory -Force -Path $destDir | Out-Null
        $dest = Join-Path $destDir 'SKILL.md'
        Copy-Item $src $dest -Force
        Write-Host "installed [$label] -> $dest"
    }

    $installed = 0
    if ($env:CGR_SKILL_DIR) {
        Install-SkillTo 'custom' $env:CGR_SKILL_DIR
        $installed++
    }

    if ($env:CGR_SKILL_TARGETS) {
        $rawTargets = $env:CGR_SKILL_TARGETS -split ',' | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ }
        if ($rawTargets -contains 'all') { $rawTargets = @('claude', 'codex', 'antigravity', 'cursor', 'opencode') }
        foreach ($t in ($rawTargets | Select-Object -Unique)) {
            if ($t -eq 'claude-code') { $t = 'claude' }
            if ($t -eq 'gemini') { $t = 'antigravity' }
            if (-not $roots.Contains($t)) { throw "unknown target: $t" }
            Install-SkillTo $labels[$t] (Join-Path $roots[$t] 'skills')
            $installed++
        }
    } elseif (-not $env:CGR_SKILL_DIR) {
        foreach ($k in $roots.Keys) {
            if (Test-Path $roots[$k]) {
                Install-SkillTo $labels[$k] (Join-Path $roots[$k] 'skills')
                $installed++
            }
        }
        if ($installed -eq 0) {
            Install-SkillTo 'generic (~/.agents)' (Join-Path $HOME '.agents\skills')
            Write-Host 'note: no existing agent config dirs detected; set $env:CGR_SKILL_TARGETS="claude,codex,antigravity,cursor,opencode" or $env:CGR_SKILL_DIR to target specific agents'
        }
    }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
