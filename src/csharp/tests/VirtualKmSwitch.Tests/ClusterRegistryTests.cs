using Microsoft.Extensions.Logging.Abstractions;
using VirtualKmSwitch.Cluster;
using VirtualKmSwitch.Ipc;
using VirtualKmSwitch.Service;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class ClusterRegistryTests : IDisposable
{
    private const string Thumbprint = "0011223344556677889900112233445566778899001122334455667788990011";

    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "VirtualKmSwitchTests", Guid.NewGuid().ToString("N"));

    private string RosterPath => Path.Combine(_directory, "roster.json");

    private ClusterRegistry Create() => new(
        NullLogger<ClusterRegistry>.Instance,
        new ClusterStore(RosterPath),
        new LocalSettingsStore(Path.Combine(_directory, "local.json")),

        // Stubbed so the test does not create a certificate in a Windows store and leave it on
        // whatever machine ran it.
        _ => Thumbprint);

    [Fact]
    public void AFreshMachineIsDescribedAsBelongingToNothing()
    {
        ClusterView view = Create().Describe();

        Assert.False(view.HasCluster);
        Assert.Empty(view.Machines);
        Assert.Equal(ClusterHealth.Ok, view.Health);
        Assert.NotEqual(Guid.Empty, view.LocalNodeId);
    }

    [Fact]
    public void CreatingAClusterPutsThisMachineInTheDescribedRoster()
    {
        ClusterRegistry registry = Create();
        registry.CreateCluster("Workshop");

        ClusterView view = registry.Describe();

        Assert.True(view.HasCluster);
        Assert.Equal("Workshop", view.ClusterName);
        Assert.NotEmpty(view.ClusterId);

        MachineInfo only = Assert.Single(view.Machines);
        Assert.True(only.IsLocal);
        Assert.Equal(view.LocalNodeId, only.NodeId);
        Assert.Equal(Environment.MachineName, only.DisplayName);
        Assert.Equal(Thumbprint, only.Thumbprint);
    }

    [Fact]
    public void TheJoinKeyIsWithheldUnlessItIsExplicitlyAskedFor()
    {
        // Every routine membership read goes to the controller, so a key attached by default
        // would sit in an unelevated process's memory for the whole session — for the sake of a
        // string that is only useful in the seconds after it is created.
        ClusterRegistry registry = Create();
        string created = registry.CreateCluster("Workshop");

        Assert.Null(registry.Describe().JoinKey);
        Assert.Equal(created, registry.Describe(includeJoinKey: true).JoinKey);
    }

    [Fact]
    public void TheClusterIdDoesNotRevealTheJoinKey()
    {
        // Beacons carry the id in the clear, so anything recoverable from it is public.
        ClusterRegistry registry = Create();
        string key = registry.CreateCluster("Workshop");

        ClusterView view = registry.Describe(includeJoinKey: true);

        Assert.DoesNotContain(JoinKeyWithoutDashes(key), view.ClusterId, StringComparison.OrdinalIgnoreCase);
        Assert.Equal(16, view.ClusterId.Length);
    }

    [Fact]
    public void AMachineThatIsNotAMemberCannotBeRemoved()
    {
        ClusterRegistry registry = Create();
        registry.CreateCluster("Workshop");

        Assert.False(registry.RemoveMachine(Guid.NewGuid()));
    }

    [Fact]
    public void LeavingIsDescribedAsBelongingToNothingAgain()
    {
        ClusterRegistry registry = Create();
        registry.CreateCluster("Workshop");
        registry.LeaveCluster();

        ClusterView view = registry.Describe(includeJoinKey: true);

        Assert.False(view.HasCluster);
        Assert.Empty(view.Machines);
        Assert.Null(view.JoinKey);
    }

    [Fact]
    public void ADamagedRosterIsReportedRatherThanPresentedAsAnEmptyCluster()
    {
        // An empty roster and an unreadable one look identical on screen, and only one of them
        // means every peer has genuinely left. This also covers the first roster ever written,
        // which is the copy a freshly joined machine depends on.
        Create().CreateCluster("Workshop");
        File.WriteAllText(RosterPath, "{ not json");

        ClusterView view = Create().Describe();

        Assert.Equal(ClusterHealth.RecoveredFromBackup, view.Health);
        Assert.NotNull(view.HealthDetail);
    }

    [Fact]
    public void AnUnreadableRosterIsReportedAsSuch()
    {
        ClusterStore store = new(RosterPath);
        Create().CreateCluster("Workshop");

        File.WriteAllText(RosterPath, "{ not json");
        File.WriteAllText(store.BackupPath, "{ also not json");

        ClusterView view = Create().Describe();

        Assert.Equal(ClusterHealth.Unreadable, view.Health);
        Assert.False(view.HasCluster);
    }

    private static string JoinKeyWithoutDashes(string key) => key.Replace("-", string.Empty);

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
