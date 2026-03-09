---
default: patch
---

Auto-retry Hello handshake for peers stuck without slot assignment.

When ICE connects but the Hello exchange on the sync DataChannel fails (e.g. due to TURN server timeouts during ICE gathering), audio can flow while the session tab shows no slots. A new watchdog tier detects active peers with no identity: re-sends Hello after 5 seconds (soft retry) and forces a reconnect after 15 seconds (hard retry). This eliminates the need to manually disconnect and reconnect to recover slot assignments.
