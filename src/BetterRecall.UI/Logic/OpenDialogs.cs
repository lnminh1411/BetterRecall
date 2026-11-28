// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Logic;

/// <summary>
/// Dialogs that show decrypted content (the Describe preview's keyframe, the in-app replay player). They live in the
/// window's popup layer, not in the page the lock drops, so lock, panic and hiding the window close them explicitly
/// (C2). Owner is the window (XamlRoot) a dialog belongs to; null hides every one. UI thread only.
/// </summary>
public sealed class OpenDialogs<T> where T : class
{
    private readonly List<(T Dialog, object? Owner)> _open = [];

    public int Count => _open.Count;

    /// <summary>Tracks a dialog until the returned handle is disposed (after ShowAsync returns).</summary>
    public IDisposable Track(T dialog, object? owner)
    {
        _open.Add((dialog, owner));
        return new Handle(this, dialog);
    }

    /// <summary>Hides the tracked dialogs of owner (all when owner is null). Their ShowAsync then returns "none", and the
    /// caller drops the image or player as on a normal close.</summary>
    public void HideAll(Action<T> hide, object? owner = null)
    {
        foreach (var (d, o) in _open.ToList())
            if (owner is null || ReferenceEquals(o, owner))
                try { hide(d); } catch { }
    }

    private sealed class Handle(OpenDialogs<T> set, T dialog) : IDisposable
    {
        public void Dispose() => set._open.RemoveAll(x => ReferenceEquals(x.Dialog, dialog));
    }
}
