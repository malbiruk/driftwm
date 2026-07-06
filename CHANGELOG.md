# Changelog

- The compositor no longer crashes when the primary GPU's renderer is
  unavailable (e.g. after unplugging the primary GPU): dmabuf imports fail
  cleanly and `driftwm msg screenshot` returns an error instead of aborting.
- Screencopy and image-copy-capture to shm buffers now read pixels back on the
  GPU that owns the offscreen texture, fixing captures of secondary-GPU
  outputs.
- Blur is disabled on outputs driven by a secondary GPU: its render passes
  cannot span two GPU contexts, so those windows render without blur instead
  of with a broken one.
