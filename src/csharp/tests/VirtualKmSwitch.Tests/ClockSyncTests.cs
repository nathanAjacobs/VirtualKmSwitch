using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// The offset estimator, driven with synthetic timestamps so the answer is known exactly.
///
/// Worth testing precisely rather than approximately: every latency figure the system reports is
/// this number subtracted from another, so an error here does not surface as a wrong measurement,
/// it surfaces as a measurement that looks right.
/// </summary>
public class ClockSyncTests
{
    /// <summary>
    /// Builds one round trip for a peer whose clock reads <paramref name="remoteAhead"/>
    /// microseconds later than ours, with the given one-way delay in each direction.
    /// </summary>
    private static (long T1, long T2, long T3, long T4) RoundTrip(
        long localSend, long remoteAhead, long delayOut, long remoteThinking, long delayBack)
    {
        long t1 = localSend;
        long t2 = localSend + delayOut + remoteAhead;
        long t3 = t2 + remoteThinking;
        long t4 = t3 - remoteAhead + delayBack;
        return (t1, t2, t3, t4);
    }

    [Fact]
    public void ASymmetricRoundTripRecoversTheOffsetExactly()
    {
        var sync = new ClockSync();
        (long t1, long t2, long t3, long t4) = RoundTrip(
            localSend: 1_000_000, remoteAhead: 250_000, delayOut: 300, remoteThinking: 50, delayBack: 300);

        sync.Record(t1, t2, t3, t4);

        // Signed as local minus remote, because that is how callers use it: a peer timestamp plus
        // the offset is that instant on our clock.
        Assert.Equal(-250_000, sync.OffsetMicroseconds);
        Assert.Equal(600, sync.RoundTripMicroseconds);
        Assert.True(sync.HasSample);
    }

    [Fact]
    public void TheResponderIsNotChargedForItsOwnThinkingTime()
    {
        var sync = new ClockSync();

        // A peer that sat on the ping for 5 ms before replying. That is not network delay, and
        // counting it would halve into the offset and skew every later measurement.
        (long t1, long t2, long t3, long t4) = RoundTrip(
            localSend: 0, remoteAhead: 0, delayOut: 200, remoteThinking: 5_000, delayBack: 200);

        sync.Record(t1, t2, t3, t4);

        Assert.Equal(0, sync.OffsetMicroseconds);
        Assert.Equal(400, sync.RoundTripMicroseconds);
    }

    [Fact]
    public void TheLowestRoundTripSampleWins()
    {
        var sync = new ClockSync();

        // A congested sample first. Its delays are lopsided, so its offset is wrong by half the
        // asymmetry — which is exactly the error the minimum-round-trip rule is there to reject.
        (long t1, long t2, long t3, long t4) = RoundTrip(
            localSend: 0, remoteAhead: 1_000, delayOut: 9_000, remoteThinking: 0, delayBack: 1_000);
        sync.Record(t1, t2, t3, t4);
        long congested = sync.OffsetMicroseconds;

        (t1, t2, t3, t4) = RoundTrip(
            localSend: 100_000, remoteAhead: 1_000, delayOut: 150, remoteThinking: 0, delayBack: 150);
        sync.Record(t1, t2, t3, t4);

        Assert.NotEqual(congested, sync.OffsetMicroseconds);
        Assert.Equal(-1_000, sync.OffsetMicroseconds);
        Assert.Equal(300, sync.RoundTripMicroseconds);
        Assert.Equal(2, sync.SampleCount);
    }

    [Fact]
    public void AWorseSampleDoesNotDisplaceABetterOne()
    {
        var sync = new ClockSync();

        (long t1, long t2, long t3, long t4) = RoundTrip(
            localSend: 0, remoteAhead: 500, delayOut: 100, remoteThinking: 0, delayBack: 100);
        sync.Record(t1, t2, t3, t4);

        (t1, t2, t3, t4) = RoundTrip(
            localSend: 10_000, remoteAhead: 500, delayOut: 20_000, remoteThinking: 0, delayBack: 100);
        sync.Record(t1, t2, t3, t4);

        Assert.Equal(-500, sync.OffsetMicroseconds);
        Assert.Equal(200, sync.RoundTripMicroseconds);
    }

    [Fact]
    public void ASampleFromAClockThatSteppedIsIgnored()
    {
        var sync = new ClockSync();

        // The reply appears to arrive before it was sent. Nothing usable can be extracted, and
        // folding it in would corrupt an estimate that may already be good.
        sync.Record(t1: 1_000, t2: 500, t3: 9_000, t4: 1_100);

        Assert.False(sync.HasSample);
        Assert.Equal(0, sync.OffsetMicroseconds);
    }

    [Fact]
    public void UntilAnExchangeCompletesTheOffsetIsZeroAndSaysSo()
    {
        var sync = new ClockSync();

        Assert.False(sync.HasSample);
        Assert.Equal(0, sync.OffsetMicroseconds);
        Assert.Equal(0, sync.RoundTripMicroseconds);
    }

    [Fact]
    public void APingRoundTripsThroughTheWireFormat()
    {
        Span<byte> buffer = stackalloc byte[ClockSync.PayloadSize];
        ClockSync.WritePing(buffer, 123_456);

        Assert.True(ClockSync.TryRead(buffer, out HeartbeatKind kind, out long t1, out long t2, out long t3));
        Assert.Equal(HeartbeatKind.Ping, kind);
        Assert.Equal(123_456, t1);
        Assert.Equal(0, t2);
        Assert.Equal(0, t3);
    }

    [Fact]
    public void APongCarriesTheOriginatorsInstantBackUntouched()
    {
        Span<byte> buffer = stackalloc byte[ClockSync.PayloadSize];
        ClockSync.WritePong(buffer, originatorSend: 111, localReceive: 222, localSend: 333);

        Assert.True(ClockSync.TryRead(buffer, out HeartbeatKind kind, out long t1, out long t2, out long t3));
        Assert.Equal(HeartbeatKind.Pong, kind);

        // The originator's instant must survive verbatim: it is on the originator's clock, and
        // reinterpreting it on the responder's would be the very error being corrected for.
        Assert.Equal(111, t1);
        Assert.Equal(222, t2);
        Assert.Equal(333, t3);
    }

    [Theory]
    [InlineData(0)]
    [InlineData(ClockSync.PayloadSize - 1)]
    [InlineData(ClockSync.PayloadSize + 1)]
    public void AWronglySizedHeartbeatIsRefused(int size)
    {
        Assert.False(ClockSync.TryRead(new byte[size], out _, out _, out _, out _));
    }

    [Fact]
    public void AnUnknownHeartbeatKindIsRefused()
    {
        byte[] payload = new byte[ClockSync.PayloadSize];
        payload[0] = 7;

        Assert.False(ClockSync.TryRead(payload, out _, out _, out _, out _));
    }
}

/// <summary>The wire clock itself.</summary>
public class MonotonicClockTests
{
    [Fact]
    public void ConversionSurvivesATimestampFromAMachineThatHasBeenUpForYears()
    {
        // The obvious form, ticks * 1_000_000 / Frequency, overflows a long after about eleven
        // days at 10 MHz. That is not an edge case for a service that starts at boot and is
        // expected never to be restarted, and the symptom is a negative latency, not a crash.
        long threeYearsOfTicks = System.Diagnostics.Stopwatch.Frequency * 60L * 60 * 24 * 365 * 3;

        long microseconds = MonotonicClock.ToMicroseconds(threeYearsOfTicks);

        Assert.True(microseconds > 0, $"conversion overflowed, giving {microseconds}");

        // Within a second over three years. Exact equality would be asserting that the machine's
        // tick frequency divides a million, which is true here and not guaranteed anywhere.
        long expectedSeconds = 60L * 60 * 24 * 365 * 3;
        Assert.InRange(microseconds / 1_000_000, expectedSeconds - 1, expectedSeconds + 1);
    }

    [Fact]
    public void TheClockMovesForwards()
    {
        long first = MonotonicClock.Microseconds;
        Thread.Sleep(2);
        long second = MonotonicClock.Microseconds;

        Assert.True(second > first);
        Assert.True(second - first < 1_000_000, "two milliseconds should not read as a second");
    }
}
