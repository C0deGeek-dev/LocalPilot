## Three.js scenes
- Use the Three.js version the project pins; APIs changed between releases.
- Keep to the scene's existing structure: how it creates the renderer, camera, loop and assets.
- Wait for assets to load before judging a scene. Errors in the console and failed asset requests are evidence.
- A canvas that is not blank is not a working scene. For a game, observe the changed behaviour: the input, the movement, the state that should follow.
