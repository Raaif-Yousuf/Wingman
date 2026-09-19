<#
.SYNOPSIS
  Renames the repo folder from copilot-ask to wingman, and moves Claude Code's
  per-project state (transcripts and the memory directory) to match.

.DESCRIPTION
  This cannot be done from inside a Claude Code session: Windows holds an open
  handle on a process's current directory, so the rename fails with a sharing
  violation while any shell, editor or agent is sitting in the folder. Run this
  from a PowerShell window whose current directory is somewhere else, with no
  Claude Code session, no cargo build and no editor open on the repo.

  What it changes:
    C:\Users\raaif\copilot-ask                               -> C:\Users\raaif\wingman
    ~\.claude\projects\C--Users-raaif-copilot-ask            -> ...\C--Users-raaif-wingman
    the absolute CARGO_TARGET_DIR path in the two filing-findings SKILL.md copies
    the "Repo:" line in CLAUDE.md and AGENTS.md

  What it deliberately does NOT change:
    %APPDATA%\copilot-ask\config.toml            (the pre-rename config, issue #1
                                                  copies it forward and leaves it)
    docs/superpowers/specs/2026-09-14-copilot-ask-design.md and the other spec
                                                 filenames (they are dated records)
    the package identity RaaifYousuf.CopilotAsk  (issue #10)

.NOTES
  Run with -WhatIf first.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$OldPath = 'C:\Users\raaif\copilot-ask',
    [string]$NewPath = 'C:\Users\raaif\wingman'
)

$ErrorActionPreference = 'Stop'

function Fail($message) { Write-Error $message; exit 1 }
function Say($message) { Write-Information $message -InformationAction Continue }

if (-not (Test-Path -LiteralPath $OldPath)) { Fail "Source folder not found: $OldPath" }
if (Test-Path -LiteralPath $NewPath) { Fail "Target already exists: $NewPath" }

# 1. Refuse to run from inside the folder being renamed.
if ((Get-Location).Path -like "$OldPath*") {
    Fail "Current directory is inside $OldPath. cd somewhere else first (for example C:\Users\raaif) and re-run."
}

# 2. Refuse while anything holds the folder open.
$holders = Get-Process -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($OldPath, [StringComparison]::OrdinalIgnoreCase) }
if ($holders) {
    $holders | ForEach-Object { "  $($_.ProcessName) (pid $($_.Id)) $($_.Path)" }
    Fail 'Processes are running from inside the folder. Close them and re-run.'
}

# 3. Refuse while git worktrees exist: their gitdir files carry absolute paths
#    and a rename orphans every one of them.
$worktrees = & git -C $OldPath worktree list --porcelain 2>$null |
    Select-String '^worktree ' | ForEach-Object { $_.Line -replace '^worktree ', '' }
if (@($worktrees).Count -gt 1) {
    $worktrees | ForEach-Object { "  $_" }
    Fail 'Extra git worktrees exist. Merge or drop them, then `git worktree remove` each and `git worktree prune`, then re-run.'
}

# 4. Warn (do not block) on uncommitted work, so nothing is silently lost.
$dirty = & git -C $OldPath status --porcelain
if ($dirty) {
    Write-Warning "Working tree is not clean. The rename preserves it, but commit first if you care about the reflog:"
    $dirty | Select-Object -First 20 | ForEach-Object { Write-Warning "  $_" }
}

# 5. The rename itself.
if ($PSCmdlet.ShouldProcess($OldPath, "Rename to $NewPath")) {
    Move-Item -LiteralPath $OldPath -Destination $NewPath
    Say "renamed  $OldPath -> $NewPath"
}

# 6. Claude Code keys its per-project state (transcripts, the memory directory)
#    on the working directory with separators replaced by dashes. Move it so the
#    memory written over the last three sessions is still found after the rename.
$oldKey = $OldPath -replace '[:\\/]', '-'   # C:\Users\raaif\copilot-ask -> C--Users-raaif-copilot-ask
$newKey = $NewPath -replace '[:\\/]', '-'
$projects = Join-Path $env:USERPROFILE '.claude\projects'
$oldProject = Join-Path $projects $oldKey
$newProject = Join-Path $projects $newKey

if (-not (Test-Path -LiteralPath $oldProject)) {
    Write-Warning "No Claude project directory at $oldProject; nothing to move."
} elseif (Test-Path -LiteralPath $newProject) {
    Write-Warning "$newProject already exists. Merge $oldProject into it by hand; refusing to overwrite."
} elseif ($PSCmdlet.ShouldProcess($oldProject, "Rename to $newProject")) {
    Move-Item -LiteralPath $oldProject -Destination $newProject
    Say "renamed  $oldProject -> $newProject"
}

# 7. Fix the absolute paths that live inside the repo.
$edits = @(
    @{ File = '.claude\skills\filing-findings\SKILL.md'; From = 'C:/Users/raaif/copilot-ask/target/wt/'; To = 'C:/Users/raaif/wingman/target/wt/' }
    @{ File = '.agents\skills\filing-findings\SKILL.md'; From = 'C:/Users/raaif/copilot-ask/target/wt/'; To = 'C:/Users/raaif/wingman/target/wt/' }
    @{ File = 'CLAUDE.md'; From = '**Repo:** `C:\Users\raaif\copilot-ask`'; To = '**Repo:** `C:\Users\raaif\wingman`' }
    @{ File = 'AGENTS.md'; From = '**Repo:** `C:\Users\raaif\copilot-ask`'; To = '**Repo:** `C:\Users\raaif\wingman`' }
)
foreach ($e in $edits) {
    $path = Join-Path $NewPath $e.File
    if (-not (Test-Path -LiteralPath $path)) { Write-Warning "skipped  $($e.File) (not found)"; continue }
    $text = Get-Content -LiteralPath $path -Raw
    if ($text -notlike "*$($e.From)*") { Write-Warning "skipped  $($e.File) (pattern already updated or changed)"; continue }
    if ($PSCmdlet.ShouldProcess($path, 'Rewrite path reference')) {
        [IO.File]::WriteAllText($path, $text.Replace($e.From, $e.To))
        Say "patched  $($e.File)"
    }
}

# 8. Stale build artifacts carry the old absolute path. Cheaper to rebuild than
#    to debug a mismatch, and sccache makes the rebuild fast.
$target = Join-Path $NewPath 'target'
if (Test-Path -LiteralPath $target) {
    Say ''
    Say "Build artifacts under $target still reference the old path."
    Say "Delete them when convenient:  Remove-Item -Recurse -Force '$target'"
}

Say ''
Say 'Done. Remaining manual steps:'
Say "  1. cd $NewPath and run: git status   (confirm the repo is intact)"
Say '  2. Commit the two SKILL.md and two .md path fixes.'
Say '  3. Reopen Claude Code from the new folder; it will pick up the moved memory directory.'
Say '  4. The GitHub repo is already named Wingman; nothing to do there.'
