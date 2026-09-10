# gaze-clicks

The real mouse and the accessibility tree, on threads of their own. What is left of the
passive click collector after 2026-09-10, when it was removed with the residual model
it collected for (git history has the collector, its recogniser and the session format).

## The mouse reader

`mouse::MouseReader` reads the mouse the user is actually working with, **read-only
and never grabbed**, so every press still reaches the compositor. It opens every evdev
node that looks like a mouse (a left button, not a keyboard) plus anything matching
the configured name, because a remapper grabs its source node and a grabbed node is
silent to other readers: for any physical press exactly one open node speaks. A rescan
picks up nodes that appear mid-run (a re-enumeration, a wireless reconnect).
`gaze-inject`'s own virtual pointer is never read; those presses are the gaze clicking.

The session (`gaze-proto/src/feedback.rs`) hands each press, with the pointer position
at that moment, to the ET5's online offset as a gaze label.

```sh
cargo run --bin gaze-clicks-cli -- devices             # which nodes a session reads
cargo run --bin gaze-clicks-cli -- presses --seconds 10 # the presses, as the session sees them
```

Reading a node needs an ACL on it for the user (`input` group).

## The tree thread

`tree::TreeService` answers "what is under this point" and "what scrolls under this
point" from the accessibility tree (`gaze-a11y`) on a thread the caller polls with a
short timeout, so a blocked D-Bus call never stalls the sample loop. The thread is
watched and replaced when a call wedges it. The session's verifier and edge scroller
both ask it.
