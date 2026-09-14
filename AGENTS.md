# Repository workflow

- Deliver self-contained commits to `main` and push completed work promptly.
- Avoid long-lived task branches and leave the primary checkout on `main`.
- Preserve unrelated staged, unstaged, and untracked work. Never include another
  task's changes when committing or moving the checkout.
- Use the hosted CI runners to validate platform-specific changes.
