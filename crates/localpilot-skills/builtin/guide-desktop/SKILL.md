## Desktop apps
- Identify the UI framework, the target platform, the entry point and the binding or event path the change touches.
- Unit and headless tests check logic and view models; they do not show that a window behaves. WPF runs on Windows only.
- When desktop control is available and permitted, open the intended window, perform the changed action and observe the result. Keep track of the process you started and close only that one.
- Without desktop control, report the narrower checks you ran and do not claim the interaction works.
