// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Capture;

/// <summary>
/// Pre-grab change gate of one output: did any frame arrive, and how much of the screen got dirty, since the last grab
/// of this kind. Keyframes and replay samples keep one each (A8): a sample-only grab used to reset the keyframe gate,
/// so the keyframe of the same change was then skipped as "no change" and the final screen never reached the timeline.
/// </summary>
internal sealed class ChangeGate
{
    private bool _frames = true;
    private double _dirtyPct = 100;

    /// <summary>A new frame came in with this share of the screen dirty (100 when unknown).</summary>
    public void Frame(double dirtyPct)
    {
        _frames = true;
        _dirtyPct = Math.Min(100, _dirtyPct + dirtyPct);
    }

    /// <summary>Duplication restarted: everything counts as changed.</summary>
    public void Invalidate()
    {
        _frames = true;
        _dirtyPct = 100;
    }

    public bool Changed(double minPercent) => _frames && _dirtyPct >= minPercent;

    public void Reset()
    {
        _frames = false;
        _dirtyPct = 0;
    }
}
