// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Contracts;

// Every key and default from docs/v1-design.md section 8. Validation lives in Core.
// Property names serialise camelCase: section "capture", key "intervalMs" = capture.intervalMs.

public sealed class Settings
{
    public CaptureSettings Capture { get; set; } = new();
    public KeyframeSettings Keyframes { get; set; } = new();
    public ReplaySettings Replay { get; set; } = new();
    public IndexSettings Index { get; set; } = new();
    public RetentionSettings Retention { get; set; } = new();
    public StorageSettings Storage { get; set; } = new();
    public AudioSettings Audio { get; set; } = new();
    public PrivacySettings Privacy { get; set; } = new();
    public InferenceSettings Inference { get; set; } = new();
    public GovernanceSettings Governance { get; set; } = new();
    public SecuritySettings Security { get; set; } = new();
    public HotkeySettings Hotkeys { get; set; } = new();
    public IpcSettings Ipc { get; set; } = new();
    public McpSettings Mcp { get; set; } = new();
    public LogSettings Log { get; set; } = new();
    public UiSettings Ui { get; set; } = new();
}

public sealed class UiSettings
{
    public string Theme { get; set; } = "system"; // system | light | dark (UI windows; the tray icon follows the taskbar)
}

public sealed class CaptureSettings
{
    public bool Consented { get; set; }
    public bool Enabled { get; set; } = true;
    public int IntervalMs { get; set; } = 2000;
    public int[] Monitors { get; set; } = [];
    public int MaxWidth { get; set; } = 1280;
    public int DhashThreshold { get; set; } = 6;
    /// <summary>Change score (percent of the screen, text-like areas count 5x) against the previous stored keyframe
    /// that a timed capture needs. Event triggers and window switches skip it.</summary>
    public double DirtyMinPercent { get; set; } = 15;
    public bool EventTriggers { get; set; } = true;
    public bool PauseOnFullscreen { get; set; } = true;
}

public sealed class KeyframeSettings
{
    public bool Enabled { get; set; } = true;          // persist keyframe images
    public string Format { get; set; } = "avif";       // avif | webp | jxl | jpeg (avif and jxl fall back to webp when their codec is missing)
    public int Quality { get; set; } = 75;
    public string Encoder { get; set; } = "auto";       // auto | hardware | software: AVIF on the iGPU's AV1 encoder, then libaom
    public int ThumbCacheMb { get; set; } = 64;        // RAM-only preview cache in the agent
}

public sealed class ReplaySettings
{
    public bool Enabled { get; set; } = true;
    public int Seconds { get; set; } = 120;
    /// <summary>Allowed replay.fps values. Capture never runs faster than the monitor's refresh rate.</summary>
    public static readonly int[] FpsChoices = [10, 15, 24, 25, 30, 48, 50, 60, 72, 75, 90, 100, 120, 144, 165, 240];

    public int Fps { get; set; } = 30;                 // one of FpsChoices
    public int BitrateKbps { get; set; } = 6000;       // per encoded video: the canvas (composited) or each monitor (perMonitor)
    public bool Audio { get; set; } = true;
    public string Encoder { get; set; } = "auto";      // auto (av1, then hevc, then h264) | av1 | hevc | h264
    public string EncoderAdapter { get; set; } = "auto"; // auto (integrated GPU first) | substring of the adapter name
    public string Layout { get; set; } = "composited";  // composited (one video, all monitors) | perMonitor (one video per monitor)
    public string Resolution { get; set; } = "native";  // native | 2160p | 1440p | 1080p | 900p | 720p | 540p | scale
    public int ScalePercent { get; set; } = 100;        // 25..100, used when Resolution = scale
    public bool DuringFullscreen { get; set; } = true;
    public bool EncryptExports { get; set; } = true;
    public string ExportDir { get; set; } = "";        // empty = <data>/replays
    public bool Notify { get; set; } = true;           // toast after a save, click opens the UI replay list
}

public sealed class IndexSettings
{
    public int Dim { get; set; } = 768;                // 768 | 512 | 256

    /// <summary>Hits with no keyword match and a cosine below this are labelled "weak match" (never hidden).
    /// 0.65 from the retrieval test: right screens scored 0.68-0.80, wrong and off-topic ones up to 0.61.</summary>
    public double WeakMatchBelow { get; set; } = 0.65;  // 0..1, 0 turns labelling off
}

public sealed class RetentionSettings
{
    public int Days { get; set; } = 30;                 // hard limit: everything older is deleted
    public int ProgressiveAfterDays { get; set; } = 7;  // 0 = off; from this age, thin captures (smallest change first)
    public int ProgressiveOverDays { get; set; } = 30;  // over this many days the thinning reaches window/app switches only
}

public sealed class StorageSettings
{
    public int MinFreeGb { get; set; } = 25;            // 0 = off; below it capture pauses (reason storage) or, with overwrite, old captures go
    public int MaxGb { get; set; } = 20;                // store.db + keyframes, live data; replaces keyframes.maxGb
    public bool Overwrite { get; set; }                 // delete oldest captures (progressive order) instead of pausing
}

public sealed class AudioSettings
{
    public bool System { get; set; }
    public bool Mic { get; set; }
    public string Language { get; set; } = "auto";
    public int MaxSegmentS { get; set; } = 30;
}

public sealed class PrivacySettings
{
    public string[] DenyProcesses { get; set; } =
    [
        "1Password.exe", "Bitwarden.exe", "KeePass.exe", "KeePassXC.exe", "Dashlane.exe",
        "LastPass.exe", "ProtonPass.exe", "Enpass.exe", "RoboForm.exe",
        "mstsc.exe", "VMConnect.exe", "msrdc.exe", "Windows365.exe", "CredentialUIBroker.exe",
        "consent.exe", "LogonUI.exe",
    ];
    public string[] DenyPathPrefixes { get; set; } = [];
    public string[] DenyTitlePatterns { get; set; } = ["InPrivate", "Incognito", "Private Browsing", "Private browsing"];
    public bool HonorDisplayAffinity { get; set; } = true;
    public bool PauseOnLock { get; set; } = true;
    public bool BrowserBadgeDetection { get; set; } = true;
    public bool NsfwFilter { get; set; } = true;
    public double NsfwThreshold { get; set; } = 0.8;
    public int NsfwGrid { get; set; } = 1;            // 1 whole screen | 2 + quadrants and centre | 3 + 3x3 grid
    public bool NsfwDetector { get; set; } = true;    // second model (NudeNet detector): exposed and covered body parts
    public double NsfwDetectorThreshold { get; set; } = 0.4;
    public string NsfwDevice { get; set; } = "auto";  // auto | npu | igpu | gpu | cpu
    public string NsfwGpuAdapter { get; set; } = "auto"; // DXGI adapter description substring, or auto
    public int PanicPurgeMinutes { get; set; }
    public bool RecordSite { get; set; } = true;      // store the browser host (address bar, Chromium browsers) with each capture
    public bool SearchHistory { get; set; } = true;   // keep searches in the encrypted store; turning it off deletes them
    public int SearchHistoryMax { get; set; } = 100;
}

public sealed class InferenceSettings
{
    public string Device { get; set; } = "auto";      // auto | npu | gpu | cpu
    public string GpuAdapter { get; set; } = "auto";  // GPU name substring (OpenVINO or DXGI), or auto; used when Device is gpu
    public string ModelDir { get; set; } = "";        // empty = <data>\models
    public int ImageTokens { get; set; } = 140;       // 70 | 140 | 280
    public int IdleExitS { get; set; } = 120;
    public bool Fake { get; set; }
}

public sealed class GovernanceSettings
{
    public bool EcoQos { get; set; } = true;
    public int DeferCpuPercent { get; set; } = 70;
    public int DeferGpuPercent { get; set; } = 60;
    public bool PauseOnBattery { get; set; } = true;
}

public sealed class SecuritySettings
{
    public bool ExcludeOwnWindows { get; set; } = true;
    public bool RequireUnlock { get; set; } = true;
    public int UnlockTimeoutMin { get; set; } = 15;
    public bool MemoryIsolation { get; set; } = true;
    public bool LockOnSessionLock { get; set; } = true;
    public string KeyProtection { get; set; } = "auto"; // auto | tpm | passphrase | dpapi
}

public sealed class HotkeySettings
{
    public string Search { get; set; } = "Ctrl+Alt+Space";
    public string Pause { get; set; } = "Ctrl+Alt+P";
    public string Replay { get; set; } = "Ctrl+Alt+R";
    public string Panic { get; set; } = "Ctrl+Alt+Shift+X";
}

public sealed class IpcSettings
{
    public bool Enabled { get; set; }
    public string PipeName { get; set; } = "BetterRecall";
    public bool AllowControl { get; set; }
    public bool AllowPurge { get; set; }
    public bool AllowThumbnails { get; set; }
    public string[] AllowClients { get; set; } = [];
    public bool RequireUnlocked { get; set; } = true;
    public int MaxResults { get; set; } = 50;
    public int DefaultResults { get; set; } = 10;
    public int MaxResponseBytes { get; set; } = 262144;
    public int MaxSearchPerMinute { get; set; } = 60;
    public int MaxPauseS { get; set; } = 86400;
    public int IdleTimeoutS { get; set; } = 300;
}

public sealed class McpSettings
{
    public bool Enabled { get; set; }
}

public sealed class LogSettings
{
    public string Level { get; set; } = "info"; // debug | info | warn | error
}
