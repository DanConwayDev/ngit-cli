# Removing label assignments

Remove your own hashtag-label assignments from an issue or pull request:

```sh
ngit issue label <id> --remove-label bug
ngit pr label <id> --remove-label bug --remove-label help-wanted
```

Use `--label` to add labels in a separate command. Removal matches values
case-insensitively. Every requested value must have a removable assignment
from the current signer; otherwise nothing is published. Labels embedded in
the original issue/PR and assignments from other authors remain in effect.
The command reports the remaining effective labels.

NIP-09 deletes whole events. When an event also contains surviving labels,
ngit signs a replacement retaining its content, timestamp, other labels and
unrelated tags. Each relay must accept all replacements before ngit sends it
the deletion request. A failed replacement therefore cannot cause the old
labels to be deleted on that route. As with other publications, partial relay
availability can leave routes temporarily out of sync.

Events referencing multiple targets are refused because deleting one would
also change another entity's labels. Removal requires the same repository
permission as adding labels: the issue/PR author or a confirmed repository
member. No signer can delete another author's assignment.

JSON reports `action: "unlabelled"`, `removed_from_own_events` with the
requested values, and `labels` with the remaining effective values.

This command publishes deletion requests and updates the publisher's cache.
Other readers discover these requests during an online refresh, including
for assignments already cached. Offline readers retain their last known view.
