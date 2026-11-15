# 5. Only the person at the HUD can answer an approval

**Status:** accepted · 2026-11-15

**Decision.** The API has no route that answers an approval. A decision is bound to one request id and
one action hash; it cannot be replayed, flipped, or reused for a changed action. A timeout means
"expired", never "approved". The user may add allow rules; they never apply to high-risk actions, and
every rule-based approval is written to history as `approval.auto`.

**Why.** The token only proves a caller can read the user's files. Anything running as the user could
otherwise approve its own requests.
