using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// Trust is a projection of the roster, and these are the properties that makes it worth having.
///
/// The pins decide who may open a connection and inject keystrokes into this machine, so the
/// question is never only "does the right peer connect" but "does the wrong one stop connecting,
/// and stay stopped across a restart".
/// </summary>
public class RosterPinProjectionTests : IDisposable
{
    private const string LocalThumbprint = "1111111111111111111111111111111111111111111111111111111111111111";

    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "VirtualKmSwitchTests", Guid.NewGuid().ToString("N"));

    private ClusterMembership Create() =>
        new(new ClusterStore(Path.Combine(_directory, "roster.json")),
            new LocalSettingsStore(Path.Combine(_directory, "local.json")));

    private ClusterMembership CreateWithCluster()
    {
        ClusterMembership membership = Create();
        membership.CreateCluster("Home", "0123456789ABCDEF", "AAAA-BBBB-CCCC-DDDD", LocalThumbprint, "Desk");
        return membership;
    }

    /// <summary>Adds a peer the way a completed enrollment eventually will.</summary>
    private static Guid EnrollPeer(ClusterMembership membership, string thumbprint, string name = "Laptop")
    {
        var peerId = Guid.NewGuid();
        var theirRoster = new Roster(peerId);
        theirRoster.Enroll(peerId, thumbprint, name);
        membership.Merge(theirRoster.Snapshot());
        return peerId;
    }

    [Fact]
    public void EnrollingAPeerTrustsItsCertificate()
    {
        ClusterMembership membership = CreateWithCluster();
        const string peerThumbprint = "2222222222222222222222222222222222222222222222222222222222222222";

        Guid peerId = EnrollPeer(membership, peerThumbprint);

        Assert.True(membership.Pins.TryResolve(peerThumbprint, out Guid resolved));
        Assert.Equal(peerId, resolved);
    }

    [Fact]
    public void ThisMachinesOwnCertificateIsNotTrusted()
    {
        // Nothing dials itself, so pinning our own certificate would only mean that anything
        // holding a copy of our private key could present it back to us and be believed.
        ClusterMembership membership = CreateWithCluster();

        Assert.False(membership.Pins.IsTrusted(LocalThumbprint));
    }

    [Fact]
    public void AnUnknownCertificateIsNeverTrusted()
    {
        ClusterMembership membership = CreateWithCluster();
        EnrollPeer(membership, "2222222222222222222222222222222222222222222222222222222222222222");

        Assert.False(membership.Pins.IsTrusted(
            "3333333333333333333333333333333333333333333333333333333333333333"));
    }

    [Fact]
    public void RemovingAPeerRevokesItsCertificate()
    {
        ClusterMembership membership = CreateWithCluster();
        const string peerThumbprint = "2222222222222222222222222222222222222222222222222222222222222222";
        Guid peerId = EnrollPeer(membership, peerThumbprint);

        Assert.True(membership.RemoveMachine(peerId));

        // The point of the whole arrangement. A removal that left the pin behind would take a
        // machine off the list while it carried on being allowed to type into this one.
        Assert.False(membership.Pins.IsTrusted(peerThumbprint));
    }

    [Fact]
    public void LeavingTheClusterTrustsNobody()
    {
        ClusterMembership membership = CreateWithCluster();
        EnrollPeer(membership, "2222222222222222222222222222222222222222222222222222222222222222");

        membership.LeaveCluster();

        Assert.Equal(0, membership.Pins.Count);
    }

    [Fact]
    public void TrustSurvivesARestartWithoutAFileOfItsOwn()
    {
        const string peerThumbprint = "2222222222222222222222222222222222222222222222222222222222222222";

        ClusterMembership first = CreateWithCluster();
        Guid peerId = EnrollPeer(first, peerThumbprint);

        // A new instance over the same directory: what the service does after a reboot. The pins
        // are rebuilt from the roster that was already being written atomically, so there is no
        // second file to have been left half-written.
        ClusterMembership restarted = Create();

        Assert.True(restarted.Pins.TryResolve(peerThumbprint, out Guid resolved));
        Assert.Equal(peerId, resolved);
    }

    [Fact]
    public void ARemovalStaysRemovedAcrossARestart()
    {
        const string peerThumbprint = "2222222222222222222222222222222222222222222222222222222222222222";

        ClusterMembership first = CreateWithCluster();
        Guid peerId = EnrollPeer(first, peerThumbprint);
        first.RemoveMachine(peerId);

        // The tombstone is what carries this, so the revocation is not something a restart can
        // quietly undo.
        Assert.False(Create().Pins.IsTrusted(peerThumbprint));
    }

    [Fact]
    public void APeerThatReEnrolsWithANewCertificateLosesTheOldOne()
    {
        ClusterMembership membership = CreateWithCluster();

        const string oldThumbprint = "2222222222222222222222222222222222222222222222222222222222222222";
        const string newThumbprint = "4444444444444444444444444444444444444444444444444444444444444444";

        var peerId = Guid.NewGuid();
        var theirRoster = new Roster(peerId);
        theirRoster.Enroll(peerId, oldThumbprint, "Laptop");
        membership.Merge(theirRoster.Snapshot());
        Assert.True(membership.Pins.IsTrusted(oldThumbprint));

        // The peer reinstalled and generated a fresh key. Rotating a compromised certificate has
        // to actually retire the old one, or the rotation bought nothing.
        theirRoster.Enroll(peerId, newThumbprint, "Laptop");
        membership.Merge(theirRoster.Snapshot());

        Assert.True(membership.Pins.IsTrusted(newThumbprint));
        Assert.False(membership.Pins.IsTrusted(oldThumbprint));
    }

    [Fact]
    public void ImportingReplacesTheSetWithoutEverLeavingALivePinAbsent()
    {
        // Import is called on every roster change while handshakes may be in flight. Clearing
        // first would open a window where a peer that is trusted before and after is refused in
        // between, which reads as a flaky network and never reproduces on demand.
        ClusterMembership membership = CreateWithCluster();
        const string kept = "2222222222222222222222222222222222222222222222222222222222222222";

        EnrollPeer(membership, kept, "Kept");
        Guid dropped = EnrollPeer(
            membership, "5555555555555555555555555555555555555555555555555555555555555555", "Dropped");

        membership.RemoveMachine(dropped);

        Assert.True(membership.Pins.IsTrusted(kept));
        Assert.Equal(1, membership.Pins.Count);
    }

    public void Dispose()
    {
        GC.SuppressFinalize(this);

        if (Directory.Exists(_directory))
        {
            Directory.Delete(_directory, recursive: true);
        }
    }
}
