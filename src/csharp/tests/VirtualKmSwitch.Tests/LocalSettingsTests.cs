using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class LocalSettingsTests : IDisposable
{
    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "VirtualKmSwitchTests", Guid.NewGuid().ToString("N"));

    private static readonly Guid MachineOne = Guid.NewGuid();
    private static readonly Guid MachineTwo = Guid.NewGuid();

    private LocalSettingsStore CreateStore() => new(Path.Combine(_directory, "local.json"));

    [Fact]
    public void AbsentSettingsLoadAsDefaultsRatherThanFailing()
    {
        LocalSettingsDocument settings = CreateStore().Load();

        Assert.Empty(settings.Hotkeys);
        Assert.True(settings.ClipboardSyncEnabled);
    }

    [Fact]
    public void BindingsRoundTrip()
    {
        LocalSettingsStore store = CreateStore();
        LocalSettingsDocument settings = store.Bind(store.Load(), 3, MachineOne);
        store.Save(settings);

        LocalSettingsDocument reloaded = store.Load();

        Assert.Equal(MachineOne, LocalSettingsStore.TargetFor(reloaded, 3));
    }

    [Fact]
    public void BindingAMachineAlreadyBoundMovesItRatherThanDuplicating()
    {
        // Otherwise a machine would appear under two keys that both claim to drive it, and which
        // one actually did would depend on iteration order.
        LocalSettingsStore store = CreateStore();

        LocalSettingsDocument settings = store.Bind(store.Load(), 1, MachineOne);
        settings = store.Bind(settings, 5, MachineOne);

        Assert.Null(LocalSettingsStore.TargetFor(settings, 1));
        Assert.Equal(MachineOne, LocalSettingsStore.TargetFor(settings, 5));
        Assert.Single(settings.Hotkeys);
    }

    [Fact]
    public void BindingAnEmptyTargetClearsTheSlot()
    {
        LocalSettingsStore store = CreateStore();

        LocalSettingsDocument settings = store.Bind(store.Load(), 2, MachineOne);
        settings = store.Bind(settings, 2, Guid.Empty);

        Assert.Null(LocalSettingsStore.TargetFor(settings, 2));
    }

    [Theory]
    [InlineData(0)]
    [InlineData(10)]
    [InlineData(-1)]
    public void OnlySlotsOneToNineAreBindable(int slot)
    {
        // Slot 0 returns control to this machine and is fixed; binding it would take away the way
        // back.
        LocalSettingsStore store = CreateStore();

        Assert.Throws<ArgumentOutOfRangeException>(() => store.Bind(store.Load(), slot, MachineOne));
    }

    [Fact]
    public void ARemovedMachineLeavesItsSlotEmptyRatherThanShiftingOthersUp()
    {
        // Silently rebinding a familiar key to a different machine is the worst outcome available:
        // the keystrokes land somewhere unexpected and nothing explains why.
        LocalSettingsStore store = CreateStore();

        LocalSettingsDocument settings = store.Bind(store.Load(), 1, MachineOne);
        settings = store.Bind(settings, 2, MachineTwo);

        settings = store.PruneRemoved(settings, new HashSet<Guid> { MachineTwo });

        Assert.Null(LocalSettingsStore.TargetFor(settings, 1));
        Assert.Equal(MachineTwo, LocalSettingsStore.TargetFor(settings, 2));
    }

    [Fact]
    public void ANewMachineTakesTheLowestFreeSlot()
    {
        LocalSettingsStore store = CreateStore();

        Assert.Equal(1, LocalSettingsStore.NextFreeSlot(store.Load()));

        LocalSettingsDocument settings = store.Bind(store.Load(), 1, MachineOne);
        Assert.Equal(2, LocalSettingsStore.NextFreeSlot(settings));

        settings = store.Bind(settings, 3, MachineTwo);
        Assert.Equal(2, LocalSettingsStore.NextFreeSlot(settings));
    }

    [Fact]
    public void AllSlotsTakenReportsNoneRatherThanAnInvalidSlot()
    {
        LocalSettingsStore store = CreateStore();
        LocalSettingsDocument settings = store.Load();

        for (int slot = LocalSettingsStore.MinSlot; slot <= LocalSettingsStore.MaxSlot; slot++)
        {
            settings = store.Bind(settings, slot, Guid.NewGuid());
        }

        Assert.Null(LocalSettingsStore.NextFreeSlot(settings));
    }

    [Fact]
    public void CorruptSettingsFallBackToDefaults()
    {
        // Unlike the roster, nothing here decides who is trusted, so losing it is an inconvenience
        // rather than a reason to refuse to start.
        LocalSettingsStore store = CreateStore();
        store.Save(store.Bind(store.Load(), 1, MachineOne));

        File.WriteAllText(store.FilePath, "{ not json");

        Assert.Empty(store.Load().Hotkeys);
    }

    [Fact]
    public void UnknownFieldsSurviveARoundTrip()
    {
        LocalSettingsStore store = CreateStore();
        store.Save(store.Bind(store.Load(), 1, MachineOne));

        string json = File.ReadAllText(store.FilePath);
        json = json.TrimEnd().TrimEnd('}') + ", \"futureOption\": true }";
        File.WriteAllText(store.FilePath, json);

        LocalSettingsDocument loaded = store.Load();
        Assert.NotNull(loaded.Extra);

        store.Save(loaded);
        Assert.Contains("futureOption", File.ReadAllText(store.FilePath));
    }

    public void Dispose()
    {
        try
        {
            if (Directory.Exists(_directory))
            {
                Directory.Delete(_directory, recursive: true);
            }
        }
        catch
        {
            // Best effort.
        }
    }
}
