# Repository workflow

- Deliver every change through a pull request into `main`; never push to
  `main` directly. Branch protection requires the `CI result` check to pass
  before merging.
- Keep pull requests self-contained and short-lived, and open them as soon as
  the work is complete.
- Preserve unrelated staged, unstaged, and untracked work. Never include another
  task's changes when committing or moving the checkout.
- Use the hosted CI runners to validate platform-specific changes.
