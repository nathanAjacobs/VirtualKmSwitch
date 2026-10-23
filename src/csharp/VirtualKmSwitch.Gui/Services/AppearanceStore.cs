using System.Text.Json;
using Avalonia;
using Avalonia.Styling;
using System.Text.Json.Serialization;

namespace VirtualKmSwitch.Gui.Services;

/// <summary>Which theme the controller should use.</summary>
public enum ThemePreference
{
    /// <summary>Follow Windows, and keep following it when it changes.</summary>
    System,

    Light,

    Dark,
}

/// <summary>What the controller remembers about how it looks.</summary>
public sealed record AppearanceDocument
{
    public int SchemaVersion { get; init; } = 1;

    public ThemePreference Theme { get; init; } = ThemePreference.System;

    [JsonExtensionData]
    public Dictionary<string, JsonElement>? Extra { get; init; }
}

/// <summary>
/// Reads and writes the controller's own appearance settings.
///
/// Deliberately not in <c>%ProgramData%</c> alongside the paired machines and hotkeys. Those describe the
/// machine — which peers it trusts, which key drives which computer — and are read by a service
/// running as SYSTEM. This describes a preference of whoever is signed in, so it lives in their
/// profile and two people sharing a machine each get their own.
/// </summary>
public sealed class AppearanceStore
{
    private static readonly JsonSerializerOptions SerializerOptions = new()
    {
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        Converters = { new JsonStringEnumConverter() },
    };

    private readonly string _path;

    public AppearanceStore()
        : this(Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "VirtualKmSwitch",
            "appearance.json"))
    {
    }

    public AppearanceStore(string path) => _path = path;

    public AppearanceDocument Load()
    {
        try
        {
            if (!File.Exists(_path))
            {
                return new AppearanceDocument();
            }

            byte[] bytes = File.ReadAllBytes(_path);
            return bytes.Length == 0
                ? new AppearanceDocument()
                : JsonSerializer.Deserialize<AppearanceDocument>(bytes, SerializerOptions)
                  ?? new AppearanceDocument();
        }
        catch (Exception)
        {
            // Falling back to the defaults rather than surfacing anything: the worst case is that
            // the window opens in the system theme, which is where it would have started anyway.
            return new AppearanceDocument();
        }
    }

    public void Save(AppearanceDocument settings)
    {
        try
        {
            string? directory = Path.GetDirectoryName(_path);
            if (!string.IsNullOrEmpty(directory))
            {
                Directory.CreateDirectory(directory);
            }

            File.WriteAllBytes(_path, JsonSerializer.SerializeToUtf8Bytes(settings, SerializerOptions));
        }
        catch (Exception)
        {
            // Losing a theme choice is not worth interrupting anyone over.
        }
    }

    /// <summary>
    /// Applies a preference to the running application.
    /// </summary>
    /// <remarks>
    /// "System" is Avalonia's default variant, which tracks Windows -- including a change made while
    /// the controller is open -- with nothing further needed here.
    /// </remarks>
    public static void Apply(ThemePreference preference)
    {
        if (Application.Current is { } application)
        {
            application.RequestedThemeVariant = preference switch
            {
                ThemePreference.Light => ThemeVariant.Light,
                ThemePreference.Dark => ThemeVariant.Dark,
                _ => ThemeVariant.Default,
            };
        }
    }
}
