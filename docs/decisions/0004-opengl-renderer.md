# 4. OpenGL (glow) renderer instead of wgpu

**Status:** accepted · 2026-11-15

**Context.** eframe's default renderer is wgpu. Idle memory was about 140 MB against a 100 MB target.

**Decision.** Build eframe with the `glow` feature only, and repaint only while the window is visible.

**Why.** Measured, not assumed: see the Performance section of `docs/IMPLEMENTATION_STATUS.md`.

**Revisit when** a platform ships without usable OpenGL.
