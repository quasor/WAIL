---
description: Merge the open "chore: prepare release" PR to trigger a release
allowed-tools: [Bash]
---

# Release

Find and merge the open release PR (titled "chore: prepare release v*").

## Instructions

1. Run: `gh pr list --state open --search "chore: prepare release v" --head release --base main --json number,title,url --limit 5`
2. If no PRs are found, tell the user there is no pending release PR.
3. If one PR is found, merge it: `gh pr merge <number> --merge --delete-branch`
4. If multiple PRs match (unlikely), list them and ask the user which one to merge.
5. Report the result with the PR URL.
