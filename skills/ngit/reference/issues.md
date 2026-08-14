# Issues — create, view, comment, close

Part of the ngit skill. Read this when working with issues.

## Commands

```bash
ngit issue create --subject "Bug title" --body "Details as markdown" --label bug --json
ngit issue create --subject "Feature" --body "..." --label enhancement --label help-wanted --json
ngit issue list --json
ngit issue list --json --status closed
ngit issue list --json --label bug
ngit issue view <ID|nevent> --json
ngit issue view <ID|nevent> --json --comments
ngit issue comment <ID|nevent> --body "Reproduced on v2.1" --json
ngit issue comment <ID|nevent> --body "Thanks!" --reply-to <comment-ID|nevent> --json
ngit issue close <ID|nevent> --reason "wontfix" --json
ngit issue resolved <ID|nevent> --reason "fixed in abc123" --json
ngit issue reopen <ID|nevent> --reason "regression in v2.3" --json
ngit issue label <ID|nevent> --label bug --label enhancement --json
ngit issue set-subject <ID|nevent> --subject "New title" --json
ngit issue set-cover-note <ID|nevent> --body "Updated description. See nostr:nevent1abc…" --json

# Existing Markdown file: normal ngit --body options accept real newlines.
ngit issue set-cover-note <ID|nevent> \
  --body "$(cat cover-note.md)" \
  --defaults --json
```

## Auto-resolve

Commits pushed to the default branch automatically resolve issues when their messages use `fixes` or `resolves` followed by a unique hex ID/prefix or `nostr:nevent1…`, for example `Fixes #deadbeef`.
