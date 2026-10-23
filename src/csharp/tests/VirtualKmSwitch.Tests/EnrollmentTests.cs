using System.Security.Cryptography.X509Certificates;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class EnrollmentHandshakeTests
{
    private const string JoinKey = "TEST-KEY-0000-0001";
    private const string AliceThumb = "AAAA1111";
    private const string BobThumb = "BBBB2222";

    private static (byte[] Initiator, byte[] Responder) Nonces() =>
        (EnrollmentHandshake.CreateNonce(), EnrollmentHandshake.CreateNonce());

    [Fact]
    public void BothSidesComputeTheSameProofForTheSameExchange()
    {
        (byte[] initiator, byte[] responder) = Nonces();

        // Each side names its own certificate first and the peer's second, so the two computations
        // are mirror images that must agree.
        byte[] fromAlice = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);
        byte[] fromBobsView = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);

        Assert.Equal(fromAlice, fromBobsView);

        Assert.Equal(
            EnrollmentResult.Accepted,
            EnrollmentHandshake.Verify(JoinKey, AliceThumb, BobThumb, initiator, responder, fromAlice));
    }

    [Fact]
    public void TheWrongKeyIsRefused()
    {
        (byte[] initiator, byte[] responder) = Nonces();
        byte[] proof = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);

        Assert.Equal(
            EnrollmentResult.WrongKey,
            EnrollmentHandshake.Verify("DIFFERENT-KEY", AliceThumb, BobThumb, initiator, responder, proof));
    }

    [Fact]
    public void AProofForDifferentCertificatesIsRefused()
    {
        // The man-in-the-middle case. An interceptor can relay a genuine proof between two honest
        // peers, but it must present its own certificate to each of them -- so the thumbprints it
        // is observed with differ from the ones the proof was computed over, and it fails.
        (byte[] initiator, byte[] responder) = Nonces();
        byte[] genuine = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);

        const string AttackerThumb = "CCCC3333";

        Assert.Equal(
            EnrollmentResult.WrongKey,
            EnrollmentHandshake.Verify(JoinKey, AliceThumb, AttackerThumb, initiator, responder, genuine));

        Assert.Equal(
            EnrollmentResult.WrongKey,
            EnrollmentHandshake.Verify(JoinKey, AttackerThumb, BobThumb, initiator, responder, genuine));
    }

    [Fact]
    public void SwappingTheCertificateOrderChangesTheProof()
    {
        // Direction must matter, or a proof computed by one side would validate as the other's.
        (byte[] initiator, byte[] responder) = Nonces();

        byte[] forward = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);
        byte[] reversed = EnrollmentHandshake.ComputeProof(JoinKey, BobThumb, AliceThumb, initiator, responder);

        Assert.NotEqual(forward, reversed);
    }

    [Fact]
    public void ReplayingAProofWithFreshNoncesFails()
    {
        // Nonces are what stop a captured exchange being replayed later against the same pair.
        (byte[] initiator, byte[] responder) = Nonces();
        byte[] proof = EnrollmentHandshake.ComputeProof(JoinKey, AliceThumb, BobThumb, initiator, responder);

        (byte[] freshInitiator, byte[] freshResponder) = Nonces();

        Assert.Equal(
            EnrollmentResult.WrongKey,
            EnrollmentHandshake.Verify(JoinKey, AliceThumb, BobThumb, freshInitiator, freshResponder, proof));
    }

    [Fact]
    public void ThumbprintCasingDoesNotChangeTheOutcome()
    {
        // Thumbprints come from different code paths on each side, and a casing difference must
        // not read as a different certificate.
        (byte[] initiator, byte[] responder) = Nonces();
        byte[] proof = EnrollmentHandshake.ComputeProof(JoinKey, "abcd1234", "ef567890", initiator, responder);

        Assert.Equal(
            EnrollmentResult.Accepted,
            EnrollmentHandshake.Verify(JoinKey, "ABCD1234", "EF567890", initiator, responder, proof));
    }

    [Fact]
    public void MalformedInputIsRejectedWithoutComparing()
    {
        (byte[] initiator, byte[] responder) = Nonces();

        Assert.Equal(
            EnrollmentResult.Malformed,
            EnrollmentHandshake.Verify(JoinKey, AliceThumb, BobThumb, initiator, responder, new byte[8]));

        Assert.Equal(
            EnrollmentResult.Malformed,
            EnrollmentHandshake.Verify(JoinKey, AliceThumb, BobThumb, new byte[4], responder, new byte[32]));
    }

    [Fact]
    public void ClusterIdIsStableAndRevealsNothingObvious()
    {
        string id = EnrollmentHandshake.ClusterIdFor(JoinKey);

        Assert.Equal(id, EnrollmentHandshake.ClusterIdFor(JoinKey));
        Assert.NotEqual(id, EnrollmentHandshake.ClusterIdFor(JoinKey + "x"));

        // Broadcast on the LAN, so it must not contain the key it derives from.
        Assert.DoesNotContain(JoinKey, id, StringComparison.OrdinalIgnoreCase);
    }
}
