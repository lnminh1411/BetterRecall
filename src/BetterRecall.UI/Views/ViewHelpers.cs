// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices.WindowsRuntime;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.Graphics;
using Windows.Storage.Streams;

namespace BetterRecall.UI.Views;

internal static class Images
{
    /// <summary>Decodes encoded image bytes in memory. Nothing touches disk.</summary>
    public static async Task<BitmapImage?> DecodeAsync(byte[] bytes, int decodeWidth = 0)
    {
        try
        {
            using var stream = new InMemoryRandomAccessStream();
            await stream.WriteAsync(bytes.AsBuffer());
            stream.Seek(0);
            var bmp = new BitmapImage();
            if (decodeWidth > 0) bmp.DecodePixelWidth = decodeWidth;
            await bmp.SetSourceAsync(stream);
            return bmp;
        }
        catch
        {
            return null;
        }
    }
}

/// <summary>
/// Loads thumbnails only for tiles the list has realised (scrolled into view), through the RAM cache.
/// A recycled tile cancels its pending request.
/// </summary>
internal sealed class ThumbLoader
{
    private readonly int _width;

    public ThumbLoader(ListViewBase view, int width = 320)
    {
        _width = width;
        view.ContainerContentChanging += OnChanging;
    }

    private void OnChanging(ListViewBase sender, ContainerContentChangingEventArgs args)
    {
        var container = args.ItemContainer;
        var image = (container.ContentTemplateRoot as FrameworkElement)?.FindName("ThumbImage") as Image;
        if (args.InRecycleQueue)
        {
            (container.Tag as CancellationTokenSource)?.Cancel();
            container.Tag = null;
            if (image is not null) image.Source = null;
            return;
        }
        if (args.Item is not CaptureItem item || image is null) return;
        if (args.Phase == 0)
        {
            image.Source = item.Thumb as ImageSource;
            if (item.Thumb is null && item.HasThumb) args.RegisterUpdateCallback(1, OnChanging);
            return;
        }
        (container.Tag as CancellationTokenSource)?.Cancel();
        var cts = new CancellationTokenSource();
        container.Tag = cts;
        _ = LoadAsync(item, container, image, cts.Token);
    }

    private async Task LoadAsync(CaptureItem item, SelectorItem container, Image image, CancellationToken ct)
    {
        var s = Services.Current;
        byte[]? bytes;
        try
        {
            bytes = await s.Thumbs.GetOrLoadAsync(item.Id, _width, async t =>
            {
                var r = await s.MediaApi.ThumbAsync(item.Id, _width, t);
                return r.Ok ? r.Value!.Bytes : null;
            }, ct);
        }
        catch (OperationCanceledException) { return; }
        if (bytes is null || ct.IsCancellationRequested) return;
        var bmp = await Images.DecodeAsync(bytes, _width);
        if (bmp is null || ct.IsCancellationRequested) return;
        item.Thumb = bmp;
        if (ReferenceEquals(container.Content, item)) image.Source = bmp;
    }
}

internal static class WindowHelpers
{
    public static nint Hwnd(Window w) => WinRT.Interop.WindowNative.GetWindowHandle(w);

    /// <summary>Mica, theme-following caption, capture exclusion.</summary>
    public static void Prepare(Window w)
    {
        w.SystemBackdrop = new MicaBackdrop();
        // No display affinity here: the window is still hidden (the warm launcher stays hidden for hours), and a
        // hidden window whose affinity changes is drawn as an empty white rectangle. Each window applies it right
        // before its first Show.
        var icon = Path.Combine(AppContext.BaseDirectory, "Assets", "app.ico");
        if (File.Exists(icon)) w.AppWindow.SetIcon(icon);
        ApplyTheme(w);
        if (Services.Current is { } s) s.ThemeChanged += () => ApplyTheme(w);
        // "system": Windows switching light/dark changes ActualTheme; caption buttons follow.
        if (w.Content is FrameworkElement r) r.ActualThemeChanged += (_, _) => ApplyTheme(w);
    }

    /// <summary>ui.theme on the window's root, plus caption button colours for windows that draw their own title bar.</summary>
    public static void ApplyTheme(Window w)
    {
        if (w.Content is not FrameworkElement root || Services.Current is not { } s) return;
        root.RequestedTheme = s.Theme;
        if (!w.ExtendsContentIntoTitleBar) return;
        bool dark = root.ActualTheme == ElementTheme.Dark;
        var tb = w.AppWindow.TitleBar;
        var fg = dark ? Colors.White : Colors.Black;
        tb.ButtonBackgroundColor = Colors.Transparent;
        tb.ButtonInactiveBackgroundColor = Colors.Transparent;
        tb.ButtonForegroundColor = fg;
        tb.ButtonHoverForegroundColor = fg;
        tb.ButtonPressedForegroundColor = fg;
        tb.ButtonInactiveForegroundColor = dark ? Windows.UI.Color.FromArgb(0xFF, 0x9A, 0x9A, 0x9A) : Windows.UI.Color.FromArgb(0xFF, 0x70, 0x70, 0x70);
        tb.ButtonHoverBackgroundColor = dark ? Windows.UI.Color.FromArgb(0x18, 0xFF, 0xFF, 0xFF) : Windows.UI.Color.FromArgb(0x18, 0, 0, 0);
        tb.ButtonPressedBackgroundColor = dark ? Windows.UI.Color.FromArgb(0x10, 0xFF, 0xFF, 0xFF) : Windows.UI.Color.FromArgb(0x10, 0, 0, 0);
    }

    /// <summary>Size in DIPs, centred on the work area of the monitor under the cursor.</summary>
    public static void CenterOnCursorMonitor(AppWindow aw, double widthDip, double heightDip, nint hwnd)
    {
        Win32.GetCursorPos(out var p);
        var area = DisplayArea.GetFromPoint(new PointInt32(p.X, p.Y), DisplayAreaFallback.Primary);
        double scale = Win32.GetDpiForWindow(hwnd) / 96.0;
        if (scale <= 0) scale = 1;
        var work = area.WorkArea;
        int w = Math.Min((int)(widthDip * scale), work.Width - 32);
        int h = Math.Min((int)(heightDip * scale), work.Height - 32);
        aw.MoveAndResize(new RectInt32(work.X + (work.Width - w) / 2, work.Y + (work.Height - h) / 3, w, h));
    }

    public static ContentDialog Dialog(XamlRoot root, string title, object content, string primary, string? close = null, bool danger = false)
    {
        var d = new ContentDialog
        {
            XamlRoot = root,
            Title = title,
            Content = content,
            PrimaryButtonText = primary,
            CloseButtonText = close ?? Loc.Get("Common_Cancel"),
            DefaultButton = danger ? ContentDialogButton.Close : ContentDialogButton.Primary,
        };
        if (danger) d.PrimaryButtonStyle = DangerButtonStyle();
        return d;
    }

    public static Style DangerButtonStyle() => (Style)Application.Current.Resources["DangerButtonStyle"];

    public static InfoBarSeverity Severity(AgentErrorKind kind) =>
        kind is AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted ? InfoBarSeverity.Error : InfoBarSeverity.Warning;
}
