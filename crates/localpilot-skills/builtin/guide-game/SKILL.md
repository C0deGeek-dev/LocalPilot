## Games
- Behaviour is what happens in the running scene: the input, the state change that follows, and what is shown.
- Prefer deterministic setups (fixed seed, fixed timestep) when reproducing a gameplay problem.
- Separate a test hook that calls game code directly from real input reaching the game.
- A build or import that succeeds is not evidence that gameplay or visuals are right.
