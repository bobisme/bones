https://github.com/steveyegge/beads
https://github.com/Dicklesworthstone/beads_rust
https://github.com/Dicklesworthstone/beads_viewer

I want to imagine recreating beads. Let's say the new project will be called "bones".

Constraints:

- beads is sqlite first and synced to a jsonl file with simple events. bones will be json events first, projected to sqlite for indexing and searching
- bones should use efficient CRDTs so merging bones changes across branches is no issue
- it should have the triage algorithms of beads_viewer built in
- it should support different issue types and states like beads, but I want to simplify and make it less jira-like and more asana-like.

Read the repos, think deeply about this, and come up with ideas that would make this amazing.
