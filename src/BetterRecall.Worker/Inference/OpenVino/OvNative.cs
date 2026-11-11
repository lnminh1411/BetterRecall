// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Worker.Inference.OpenVino;

/// <summary>P/Invoke on the OpenVINO 2026.4 C API (openvino_c.dll). Signatures from openvino/c/*.h of the same
/// release. Only non-variadic entry points are used (the *_props variants), so marshalling is plain x64 ABI.</summary>
internal static unsafe partial class OvNative
{
    private const string Lib = "openvino_c";

    public const int OK = 0;

    // ov_element_type_e (ov_common.h, 2026.4): DYNAMIC=0, BOOLEAN, BF16, F16, F32, F64, I4, I8, I16, I32, I64, ...
    public const int ET_BF16 = 2, ET_F16 = 3, ET_F32 = 4, ET_I32 = 9, ET_I64 = 10;

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_shape_t { public long rank; public long* dims; }

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_version_t { public nint buildNumber; public nint description; }

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_available_devices_t { public nint* devices; public nuint size; }

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_property_t { public nint key; public nint value; }

    [LibraryImport(Lib)] public static partial int ov_get_openvino_version(ov_version_t* version);
    [LibraryImport(Lib)] public static partial void ov_version_free(ov_version_t* version);
    [LibraryImport(Lib)] public static partial nint ov_get_last_err_msg();
    [LibraryImport(Lib)] public static partial void ov_free(nint content);

    [LibraryImport(Lib)] public static partial int ov_core_create(out nint core);
    [LibraryImport(Lib)] public static partial void ov_core_free(nint core);
    [LibraryImport(Lib)] public static partial int ov_core_get_available_devices(nint core, ov_available_devices_t* devices);
    [LibraryImport(Lib)] public static partial void ov_available_devices_free(ov_available_devices_t* devices);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    public static partial int ov_core_get_property(nint core, string deviceName, string key, out nint value);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf16)]
    public static partial int ov_core_read_model_unicode(nint core, string modelPath, string? binPath, out nint model);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    public static partial int ov_core_compile_model_props(nint core, nint model, string deviceName, nuint numProperties, ov_property_t* properties, out nint compiledModel);

    [LibraryImport(Lib)] public static partial void ov_model_free(nint model);
    [LibraryImport(Lib)] [return: MarshalAs(UnmanagedType.U1)] public static partial bool ov_model_is_dynamic(nint model);
    [LibraryImport(Lib)] public static partial int ov_model_inputs_size(nint model, out nuint size);
    [LibraryImport(Lib)] public static partial int ov_model_outputs_size(nint model, out nuint size);
    [LibraryImport(Lib)] public static partial int ov_model_const_input_by_index(nint model, nuint index, out nint port);
    [LibraryImport(Lib)] public static partial int ov_model_const_output_by_index(nint model, nuint index, out nint port);
    [LibraryImport(Lib)] public static partial int ov_port_get_any_name(nint port, out nint name);
    [LibraryImport(Lib)] public static partial int ov_const_port_get_shape(nint port, ov_shape_t* shape);
    [LibraryImport(Lib)] public static partial int ov_port_get_element_type(nint port, out int type);
    [LibraryImport(Lib)] public static partial void ov_output_const_port_free(nint port);

    [LibraryImport(Lib)] public static partial void ov_compiled_model_free(nint compiled);
    [LibraryImport(Lib)] public static partial int ov_compiled_model_create_infer_request(nint compiled, out nint request);

    [LibraryImport(Lib)] public static partial void ov_infer_request_free(nint request);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    public static partial int ov_infer_request_set_tensor(nint request, string name, nint tensor);
    [LibraryImport(Lib)] public static partial int ov_infer_request_infer(nint request);
    [LibraryImport(Lib)] public static partial int ov_infer_request_get_output_tensor_by_index(nint request, nuint index, out nint tensor);

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_dimension_t { public long min; public long max; }

    [StructLayout(LayoutKind.Sequential)]
    public struct ov_partial_shape_t { public ov_dimension_t rank; public ov_dimension_t* dims; }

    [LibraryImport(Lib)] public static partial int ov_partial_shape_create_static(long rank, long* dims, ov_partial_shape_t* shape);
    [LibraryImport(Lib)] public static partial void ov_partial_shape_free(ov_partial_shape_t* shape);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    public static partial int ov_model_reshape_input_by_name(nint model, string tensorName, ov_partial_shape_t shape);

    [LibraryImport(Lib)] public static partial int ov_shape_create(long rank, long* dims, ov_shape_t* shape);
    [LibraryImport(Lib)] public static partial int ov_shape_free(ov_shape_t* shape);
    [LibraryImport(Lib)] public static partial int ov_tensor_create(int type, ov_shape_t shape, out nint tensor);
    [LibraryImport(Lib)] public static partial int ov_tensor_get_shape(nint tensor, ov_shape_t* shape);
    [LibraryImport(Lib)] public static partial int ov_tensor_get_element_type(nint tensor, out int type);
    [LibraryImport(Lib)] public static partial int ov_tensor_get_size(nint tensor, out nuint size);
    [LibraryImport(Lib)] public static partial int ov_tensor_data(nint tensor, out nint data);
    [LibraryImport(Lib)] public static partial void ov_tensor_free(nint tensor);

    public static string LastError(int status)
    {
        string? msg = null;
        try { var p = ov_get_last_err_msg(); if (p != 0) msg = Marshal.PtrToStringUTF8(p); } catch { }
        msg = string.IsNullOrWhiteSpace(msg) ? $"status {status}" : msg.Trim();
        // OpenVINO messages carry source paths and multi-line traces; keep the first meaningful line.
        var line = msg.Split('\n').Select(l => l.Trim()).FirstOrDefault(l => l.Length > 0 && !l.StartsWith("Check '", StringComparison.Ordinal) && !l.Contains(".cpp:", StringComparison.Ordinal))
                   ?? msg.Split('\n')[0];
        return $"{line} (status {status})";
    }

    public static void Check(int status, string what)
    {
        if (status != OK) throw new OpenVinoException($"{what}: {LastError(status)}");
    }

    public static string TakeString(nint p)
    {
        if (p == 0) return "";
        try { return Marshal.PtrToStringUTF8(p) ?? ""; }
        finally { ov_free(p); }
    }
}

public sealed class OpenVinoException(string message) : Exception(message);
