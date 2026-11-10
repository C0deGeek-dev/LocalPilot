## Browser apps
- Use the project's own scripts (`dev`, `build`, `test`) and the framework versions in its manifest and lockfile.
- Reuse a dev server that is already running for this project, or start one instance and stop it when you are done.
- Wait until the page is actually served before you inspect it. Exercise the action the change is about and look at the resulting state and the console errors.
- A page that loads is not evidence that the changed action works.
- Claim that something looks right only when a screenshot reached an image-capable model and was examined. Without browser control, report the checks you could run instead.
