// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Navigation;

namespace BetterRecall.UI.Views;

public sealed class RecView(RecommendationRow row)
{
    public RecommendationRow Row { get; } = row;

    public string Setting
    {
        get
        {
            var d = SettingsSchema.Find(Row.Key);
            if (d is not null)
            {
                var label = Loc.Get(d.LabelKey);
                if (label != d.LabelKey) return label;
            }
            return Row.Key;
        }
    }

    public string Current => Display(SettingsSchema.Find(Row.Key), Row.Current);
    public string Proposed => Display(SettingsSchema.Find(Row.Key), Row.Proposed);
    public string Reason => Loc.Text(Row.Reason);
    public string AccessibleName => Loc.Format("BenchPage_RecName", Setting, Current, Proposed, Reason);

    public static Visibility Shown(string? s) => string.IsNullOrEmpty(s) ? Visibility.Collapsed : Visibility.Visible;

    private static string Display(SettingDescriptor? d, JsonNode? node)
    {
        if (node is null) return "—";
        if (node is JsonValue val)
        {
            if (val.TryGetValue<bool>(out var b))
                return Loc.Get(b ? "Common_On" : "Common_Off");

            if (d is { Type: SettingType.Choice or SettingType.IntChoice })
            {
                string s;
                if (val.TryGetValue<string>(out var str)) s = str;
                else if (SettingsSchema.TryNumber(node, out var n)) s = ((long)n).ToString(CultureInfo.InvariantCulture);
                else s = val.ToString();

                var choiceKey = d.ChoiceKey(s);
                var choiceLabel = Loc.Get(choiceKey);
                return choiceLabel != choiceKey ? choiceLabel : s;
            }

            if (val.TryGetValue<string>(out var sval))
                return d is { Type: SettingType.Adapter } && string.Equals(sval, "auto", StringComparison.OrdinalIgnoreCase)
                    ? Loc.Get(d.LabelKey + "_auto") : sval;
            if (SettingsSchema.TryNumber(node, out var num)) return num.ToString(CultureInfo.CurrentCulture);
        }
        return node.ToJsonString();
    }
}

public sealed class EncoderView(EncoderBench e)
{
    public EncoderBench E { get; } = e;

    public string Layout
    {
        get
        {
            var key = "S_replay_layout_" + E.Layout;
            var label = Loc.Get(key);
            return label != key ? label : E.Layout;
        }
    }

    public string Size => $"{E.Width} × {E.Height}";
    public string Fps => E.Fps.ToString(CultureInfo.CurrentCulture);
    public string P95 => E.Ok ? E.EncodeMsP95.ToString("0.#", CultureInfo.CurrentCulture) : "—";
    public string Cpu => E.Ok ? E.CpuPercent.ToString("0.#", CultureInfo.CurrentCulture) : "—";
    public string Mb => E.Ok ? E.WorkingSetMb.ToString(CultureInfo.CurrentCulture) : "—";
    public string Result => E.Ok ? Loc.Get("BenchPage_Ok") : (E.Error ?? "");
    public Brush ResultBrush => (Brush)Application.Current.Resources[E.Ok ? "SystemFillColorSuccessBrush" : "SystemFillColorCriticalBrush"];

    public string AccessibleName =>
        string.Join(", ", E.Adapter, CodecLabel, E.Api, Layout, Size, Fps + " fps", P95, Result);

    private string CodecLabel
    {
        get
        {
            var key = "S_replay_encoder_" + E.Codec.ToLowerInvariant();
            var label = Loc.Get(key);
            return label != key ? label : E.Codec;
        }
    }
}

public sealed partial class BenchPage : Page, IDataPage
{
    private readonly BenchViewModel _vm = new(Services.Current.Api, () => Services.Current.CreateRunClient());
    private CancellationTokenSource? _cts;
    private readonly DispatcherQueueTimer? _timer;
    private readonly Stopwatch _clock = new();
    private bool _autoRun;

    public BenchPage()
    {
        InitializeComponent();
        _timer = DispatcherQueue.CreateTimer();
        _timer.Interval = TimeSpan.FromSeconds(1);
        _timer.Tick += (_, _) => StatusText.Text = Loc.Format("BenchPage_Running", Elapsed(_clock.Elapsed));
        Loaded += (_, _) =>
        {
            if (_autoRun)
            {
                _autoRun = false;
                StartRun();
            }
            else Reload();
        };
        Unloaded += (_, _) =>
        {
            _cts?.Cancel();
            _timer?.Stop();
        };
    }

    protected override void OnNavigatedTo(NavigationEventArgs e)
    {
        _autoRun = e.Parameter as string == "run";
    }

    public void Reload()
    {
        if (_vm.IsRunning) return;
        _ = LoadAsync();
    }

    private async Task LoadAsync()
    {
        Busy.IsActive = true;
        await _vm.LoadLastAsync();
        Busy.IsActive = false;
        if (_vm.Report is not null)
            Services.Current.SetLastBench(_vm.Report);
        Render();
    }

    private async void StartRun()
    {
        if (_vm.IsRunning) return;
        _cts = new CancellationTokenSource();
        // Move focus to Cancel before disabling Run; otherwise focus falls to the next tab stop and the page scrolls away.
        CancelButton.Visibility = Visibility.Visible;
        CancelButton.UpdateLayout();
        CancelButton.Focus(FocusState.Programmatic);
        RunButton.IsEnabled = false;
        Busy.IsActive = true;
        ApplyButton.IsEnabled = false;
        ResultBar.IsOpen = false;
        _clock.Restart();
        StatusText.Text = Loc.Format("BenchPage_Running", Elapsed(_clock.Elapsed));
        _timer?.Start();
        bool ok = false;
        try
        {
            ok = await _vm.RunAsync(_cts.Token);
        }
        finally
        {
            _timer?.Stop();
            StatusText.Text = "";
            RunButton.IsEnabled = true;
            CancelButton.Visibility = Visibility.Collapsed;
            Busy.IsActive = false;
            _cts?.Dispose();
            _cts = null;
            if (ok && _vm.Report is not null)
                Services.Current.SetLastBench(_vm.Report);
            RunButton.Focus(FocusState.Programmatic);
            Render();
        }
    }

    private void Run_Click(object sender, RoutedEventArgs e) => StartRun();

    private void Cancel_Click(object sender, RoutedEventArgs e) => _cts?.Cancel();

    private async void Apply_Click(object sender, RoutedEventArgs e)
    {
        ApplyButton.IsEnabled = false;
        var r = await _vm.ApplyAsync();
        if (r is { Ok: true })
        {
            await Services.Current.RefreshSettingsAsync();
            Services.Current.ApplySecurity();
        }
        Render();
    }

    private void Render()
    {
        var m = _vm.Message;
        ResultBar.IsOpen = !m.IsEmpty;
        ResultBar.Message = Loc.Text(m);
        ResultBar.Severity = m.Key switch
        {
            "BenchPage_Applied" => InfoBarSeverity.Success,
            "BenchPage_None" or "BenchPage_NoRecs" or "BenchPage_Cancelled" or "BenchPage_ApplyNone" => InfoBarSeverity.Informational,
            _ => InfoBarSeverity.Error,
        };

        var rep = _vm.Report;
        HardwareCard.Visibility = rep is null ? Visibility.Collapsed : Visibility.Visible;
        RecsCard.Visibility = rep is null ? Visibility.Collapsed : Visibility.Visible;
        RawExpander.Visibility = rep is null ? Visibility.Collapsed : Visibility.Visible;
        if (rep is null) return;

        HardwareLines.Children.Clear();
        HardwareLines.Children.Add(new TextBlock
        {
            Text = Loc.Format("Hw_Cpu", rep.Hardware.Cpu, rep.Hardware.Cores, rep.Hardware.Threads),
            TextWrapping = TextWrapping.Wrap,
        });
        HardwareLines.Children.Add(new TextBlock
        {
            Text = Loc.Format("Hw_Ram", (rep.Hardware.RamMb / 1024.0).ToString("0.#", CultureInfo.CurrentCulture)),
            TextWrapping = TextWrapping.Wrap,
        });
        HardwareLines.Children.Add(new TextBlock
        {
            Text = rep.Hardware.HasBattery ? Loc.Get("Hw_Battery") : Loc.Get("Hw_NoBattery"),
            TextWrapping = TextWrapping.Wrap,
        });
        HardwareLines.Children.Add(new TextBlock
        {
            Text = rep.Hardware.Npu is { } npu ? Loc.Format("Hw_Npu", npu.Name) : Loc.Get("Hw_NoNpu"),
            TextWrapping = TextWrapping.Wrap,
        });
        foreach (var gpu in rep.Hardware.Gpus)
        {
            var vram = gpu.Integrated ? Loc.Get("Hw_GpuIntegrated") : Loc.Format("Hw_GpuDiscrete", (gpu.VramMb / 1024.0).ToString("0.#", CultureInfo.CurrentCulture));
            var codecs = gpu.Codecs.Length == 0
                ? Loc.Get("Hw_NoCodecs")
                : string.Join(", ", gpu.Codecs.Select(c =>
                {
                    var key = "S_replay_encoder_" + c.ToLowerInvariant();
                    var val = Loc.Get(key);
                    return val != key ? val : c.ToUpperInvariant();
                }));
            HardwareLines.Children.Add(new TextBlock
            {
                Text = Loc.Format("Hw_Gpu", gpu.Name, gpu.Vendor, vram, codecs),
                TextWrapping = TextWrapping.Wrap,
            });
        }
        foreach (var mon in rep.Hardware.Monitors)
        {
            HardwareLines.Children.Add(new TextBlock
            {
                Text = Loc.Format("Hw_Monitor", mon.Index + 1, mon.Width, mon.Height, mon.RefreshHz, mon.Adapter),
                TextWrapping = TextWrapping.Wrap,
            });
        }
        HardwareLines.Children.Add(new TextBlock
        {
            Text = Loc.Format("Hw_Os", rep.Hardware.Os),
            TextWrapping = TextWrapping.Wrap,
        });
        HardwareLines.Children.Add(new TextBlock
        {
            Text = Loc.Format("BenchPage_Measured", rep.Ts.ToLocalTime().ToString("g", CultureInfo.CurrentCulture), Elapsed(TimeSpan.FromMilliseconds(rep.TookMs))),
            TextWrapping = TextWrapping.Wrap,
            Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
        });

        var recs = _vm.Recommendations.Select(x => new RecView(x)).ToList();
        RecList.ItemsSource = recs;
        RecsHeader.Visibility = recs.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        ApplyButton.Visibility = recs.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        ApplyButton.IsEnabled = !_vm.IsRunning;

        InferenceTable.Children.Clear();
        InferenceTable.RowDefinitions.Clear();
        InferenceTable.ColumnDefinitions.Clear();
        if (_vm.Inference.Count > 0)
        {
            for (int i = 0; i < 6; i++)
                InferenceTable.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

            InferenceTable.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var headers = new[]
            {
                Loc.Get("BenchPage_ColDevice/Text"),
                Loc.Get("BenchPage_Col70/Text"),
                Loc.Get("BenchPage_Col140/Text"),
                Loc.Get("BenchPage_Col280/Text"),
                Loc.Get("BenchPage_ColText/Text"),
                Loc.Get("BenchPage_ColCosine/Text"),
            };
            var captionStyle = (Style)Application.Current.Resources["CaptionTextBlockStyle"];
            for (int c = 0; c < headers.Length; c++)
            {
                var h = new TextBlock { Text = headers[c], Style = captionStyle };
                Grid.SetRow(h, 0);
                Grid.SetColumn(h, c);
                InferenceTable.Children.Add(h);
            }

            int r = 1;
            foreach (var item in _vm.Inference)
            {
                InferenceTable.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
                var dev = new TextBlock
                {
                    Text = $"{item.Device} ({item.Ep})",
                    FontWeight = item.Chosen ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal,
                };
                Grid.SetRow(dev, r);
                Grid.SetColumn(dev, 0);
                InferenceTable.Children.Add(dev);

                if (item.Ok)
                {
                    string FormatMs(double? v) => v is { } ms
                        ? Loc.Format("BenchPage_Ms", ms.ToString("0.#", CultureInfo.CurrentCulture))
                        : "—";

                    var c70 = new TextBlock { Text = FormatMs(item.Ms70) };
                    Grid.SetRow(c70, r);
                    Grid.SetColumn(c70, 1);
                    InferenceTable.Children.Add(c70);

                    var c140 = new TextBlock { Text = FormatMs(item.Ms140) };
                    Grid.SetRow(c140, r);
                    Grid.SetColumn(c140, 2);
                    InferenceTable.Children.Add(c140);

                    var c280 = new TextBlock { Text = FormatMs(item.Ms280) };
                    Grid.SetRow(c280, r);
                    Grid.SetColumn(c280, 3);
                    InferenceTable.Children.Add(c280);

                    var cText = new TextBlock { Text = Loc.Format("BenchPage_Ms", item.TextMs.ToString("0.#", CultureInfo.CurrentCulture)) };
                    Grid.SetRow(cText, r);
                    Grid.SetColumn(cText, 4);
                    InferenceTable.Children.Add(cText);

                    var cCosine = new TextBlock { Text = item.Cosine.ToString("0.0000", CultureInfo.CurrentCulture) };
                    Grid.SetRow(cCosine, r);
                    Grid.SetColumn(cCosine, 5);
                    InferenceTable.Children.Add(cCosine);
                }
                else
                {
                    var err = new TextBlock
                    {
                        Text = item.Error ?? "",
                        Style = (Style)Application.Current.Resources["CriticalTextStyle"],
                    };
                    Grid.SetRow(err, r);
                    Grid.SetColumn(err, 1);
                    Grid.SetColumnSpan(err, 5);
                    InferenceTable.Children.Add(err);
                }
                r++;
            }
        }

        CaptureLines.Children.Clear();
        if (rep.Capture is { } cap)
        {
            CaptureLines.Children.Add(new TextBlock
            {
                Text = Loc.Format("Hw_Capture",
                    cap.TickMs.ToString("0.#", CultureInfo.CurrentCulture),
                    cap.KeyframeEncodeMs.ToString("0.#", CultureInfo.CurrentCulture),
                    cap.CrossAdapterMs.ToString("0.#", CultureInfo.CurrentCulture)),
                TextWrapping = TextWrapping.Wrap,
            });
        }
        if (rep.Search is { } search)
        {
            CaptureLines.Children.Add(new TextBlock
            {
                Text = Loc.Format("Hw_Search",
                    search.Rows,
                    search.VectorMs.ToString("0.#", CultureInfo.CurrentCulture),
                    search.HybridMs.ToString("0.#", CultureInfo.CurrentCulture)),
                TextWrapping = TextWrapping.Wrap,
            });
        }

        EncoderList.ItemsSource = rep.Encoders.Select(x => new EncoderView(x)).ToList();
    }

    private static string Elapsed(TimeSpan t) => $"{(int)t.TotalMinutes}:{t.Seconds:00}";
}
