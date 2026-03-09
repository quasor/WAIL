---
description: Merge the open "chore: prepare release" PR to trigger a release
allowed-tools: [Bash]
---

# Release

Find and merge the open release PR (titled "chore: prepare release").

## Instructions

1. Run: `gh pr list --state open --head release --base main --json number,title,url --limit 5`
2. If no PRs are found, tell the user there is no pending release PR.
3. If one PR is found, wait 120 seconds (`sleep 120`) before merging to allow CI checks to complete, then merge it: `gh pr merge <number> --merge --delete-branch`
4. If multiple PRs match (unlikely), list them and ask the user which one to merge.
5. Report the result with the PR URL.
