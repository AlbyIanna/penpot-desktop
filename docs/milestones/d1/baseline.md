# D1 — the "before" baseline

Captured **before** any flag change, because these surfaces stop existing once D1 lands.
Without this, D1's before/after is an assertion instead of evidence.

Stack: Penpot 2.16.2, ports 9046/6508/5581/6524, empty vault, single provisioned user.
Captures: `scripts/shots.sh` at a fixed 1280×800 viewport, `SETTLE_MS=15000`.

The flags live at capture time, straight from the backend boot line — all four D1 targets ON:

```
dashboard-templates-section, login-with-password, google-fonts-provider, registration
```

## The shots

| File | Surface | What it is evidence of |
|---|---|---|
| `img/before-auth-login.png` | `#/auth/login` | The login form exists, with a "Create an account" link. On a single-user offline app there is nobody to log in as. |
| `img/before-auth-register.png` | `#/auth/register` | The `registration` flag: full name / work email / password, plus a "Send me product updates" opt-in. D1 removes this. |
| `img/before-dashboard.png` | `#/dashboard/recent?team-id=…` | The `dashboard-templates-section` flag: the "Libraries & Templates" carousel. Its thumbnails are fetched from penpot.app — this is a *visible* non-loopback egress, the thing the D1 egress observer must drive to zero. |
| `img/before-dashboard-fonts.png` | `#/dashboard/fonts?team-id=…` | The team Fonts page (custom upload only). |
| `img/before-settings-profile.png` | `#/settings/profile` | The account surface: Password, Notifications, Change email, "Want to remove your account?", Release notes — all inert or misleading offline. |

## What these shots do NOT prove

Stated explicitly so a later reader does not over-read them:

- **`before-dashboard-fonts.png` is not evidence about `google-fonts-provider`.** That flag's
  surface is the font picker inside the *workspace*, not this page. The Google-fonts baseline
  has to be captured from an open file, or asserted by the gate against `config.js` + a
  rendered workspace — not inferred from this image.
- **No workspace was open for any of these.** Every shot is a dashboard/auth surface.
- **The egress claim is visual, not measured.** The carousel thumbnails clearly come from
  outside, but "zero non-loopback connection attempts" is the observer's job to *measure*.

## Facts discovered while capturing (they bind the later tasks)

1. **The dashboard route is team-scoped**: `#/dashboard/recent?team-id=<uuid>`. Bare
   `#/dashboard` renders Penpot's own 404. The team id is per-data-dir, so the gate must
   discover it at runtime, never hardcode it.
2. **`/__bootstrap` is one-shot per boot** (`apps/desktop/src/lib.rs:481`). Each `shots.sh`
   run is a fresh browser with no cookies, so exactly one run per boot can be authenticated.
   More logged-in surfaces than that ⇒ restart the stack, or capture them in one run.
3. **A short settle silently lies.** At `SETTLE_MS=3500` the dashboard screenshotted as the
   *login card* — Penpot renders that while it resolves the session. The capture "succeeded"
   and exited 0. The tell was three different surfaces producing byte-identical PNGs; the
   fix was `SETTLE_MS=15000`. Any future capture of an authenticated surface must be eyeballed
   or hash-compared, never trusted on exit code alone.
