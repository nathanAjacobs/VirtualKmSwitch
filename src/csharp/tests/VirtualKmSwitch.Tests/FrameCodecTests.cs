using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class FrameCodecTests
{
    private static readonly InputEvent[] Sample =
    [
        InputEvent.MouseMove(1000, 12, -34),
        InputEvent.MouseButton(1000, 1, up: false),
        InputEvent.Key(1000, 0x1E, InputEventFlags.Extended),
        InputEvent.MouseWheel(1000, -120, horizontal: true),
        InputEvent.ReleaseAll(1000),
    ];

    [Fact]
    public void InputBatchesRoundTrip()
    {
        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        Assert.True(FrameCodec.TryWriteInputBatch(buffer, 987654321, Sample, out int written));
        Assert.Equal(buffer.Length, written);

        Assert.Equal(
            FrameReadResult.Complete,
            FrameCodec.TryReadFrame(buffer, out FrameType type, out ReadOnlySpan<byte> payload, out int consumed));

        Assert.Equal(FrameType.InputBatch, type);
        Assert.Equal(written, consumed);

        var decoded = new InputEvent[Sample.Length];
        int count = FrameCodec.ReadInputBatch(payload, decoded, out long timestamp);

        Assert.Equal(Sample.Length, count);
        Assert.Equal(987654321, timestamp);

        for (int i = 0; i < Sample.Length; i++)
        {
            Assert.Equal(Sample[i].Kind, decoded[i].Kind);
            Assert.Equal(Sample[i].Flags, decoded[i].Flags);
            Assert.Equal(Sample[i].Code, decoded[i].Code);
            Assert.Equal(Sample[i].X, decoded[i].X);
            Assert.Equal(Sample[i].Y, decoded[i].Y);
        }
    }

    [Fact]
    public void ExtremeValuesSurviveEncoding()
    {
        // Deltas are signed and a wheel delta is negative; a width or signedness mistake here
        // would send the pointer the wrong way rather than fail visibly.
        InputEvent[] extremes =
        [
            new(long.MaxValue, InputEventKind.MouseMove, InputEventFlags.None, ushort.MaxValue, int.MinValue, int.MaxValue),
            new(long.MinValue, InputEventKind.Key, InputEventFlags.Up | InputEventFlags.E1, 0, -1, 1),
        ];

        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(extremes.Length)];
        FrameCodec.TryWriteInputBatch(buffer, -5, extremes, out _);
        FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> payload, out _);

        var decoded = new InputEvent[extremes.Length];
        FrameCodec.ReadInputBatch(payload, decoded, out long timestamp);

        Assert.Equal(-5, timestamp);
        Assert.Equal(int.MinValue, decoded[0].X);
        Assert.Equal(int.MaxValue, decoded[0].Y);
        Assert.Equal(ushort.MaxValue, decoded[0].Code);
        Assert.Equal(-1, decoded[1].X);
    }

    [Fact]
    public void PartialFramesReportIncompleteRatherThanFailing()
    {
        // TCP delivers a stream, not messages: a frame arriving in pieces is the normal case, not
        // an error, and treating it as one would drop healthy connections constantly.
        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        FrameCodec.TryWriteInputBatch(buffer, 1, Sample, out int written);

        for (int prefix = 0; prefix < written; prefix++)
        {
            Assert.Equal(
                FrameReadResult.Incomplete,
                FrameCodec.TryReadFrame(buffer[..prefix], out _, out _, out _));
        }

        Assert.Equal(FrameReadResult.Complete, FrameCodec.TryReadFrame(buffer, out _, out _, out _));
    }

    [Fact]
    public void OversizedLengthIsRejected()
    {
        // The length prefix is attacker-controlled until checked. Trusting it would let a peer
        // dictate an allocation, or read past the buffer.
        Span<byte> buffer = new byte[16];
        System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(buffer, ushort.MaxValue);
        buffer[2] = (byte)FrameType.InputBatch;

        Assert.Equal(FrameReadResult.Malformed, FrameCodec.TryReadFrame(buffer, out _, out _, out _));
    }

    [Theory]
    [InlineData(0)]    // Unknown
    [InlineData(99)]   // never defined
    [InlineData(255)]
    public void UnknownFrameTypesAreRejected(byte type)
    {
        Span<byte> buffer = new byte[8];
        System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(buffer, 1);
        buffer[2] = type;

        Assert.Equal(FrameReadResult.Malformed, FrameCodec.TryReadFrame(buffer, out _, out _, out _));
    }

    [Fact]
    public void ADeclaredCountThatDisagreesWithThePayloadIsRejected()
    {
        // Believing the count over the actual bytes would read past the payload, which is the
        // classic way a length-prefixed parser becomes an information leak.
        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        FrameCodec.TryWriteInputBatch(buffer, 1, Sample, out _);
        FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> payload, out _);

        byte[] tampered = payload.ToArray();
        System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(tampered.AsSpan(8), 9999);

        Assert.Equal(-1, FrameCodec.ReadInputBatch(tampered, new InputEvent[64], out _));
    }

    [Fact]
    public void TruncatedBatchPayloadIsRejected()
    {
        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        FrameCodec.TryWriteInputBatch(buffer, 1, Sample, out _);
        FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> payload, out _);

        Assert.Equal(-1, FrameCodec.ReadInputBatch(payload[..^3], new InputEvent[64], out _));
        Assert.Equal(-1, FrameCodec.ReadInputBatch([], new InputEvent[64], out _));
    }

    [Fact]
    public void ADestinationTooSmallIsRejectedRatherThanOverflowing()
    {
        Span<byte> buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        FrameCodec.TryWriteInputBatch(buffer, 1, Sample, out _);
        FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> payload, out _);

        Assert.Equal(-1, FrameCodec.ReadInputBatch(payload, new InputEvent[2], out _));
    }

    [Fact]
    public void ConsecutiveFramesAreReadIndependently()
    {
        // Several frames arrive in one TCP read constantly; the consumed count is what keeps the
        // reader aligned across them.
        Span<byte> buffer = new byte[512];
        FrameCodec.TryWriteInputBatch(buffer, 1, Sample.AsSpan(0, 2), out int first);
        FrameCodec.TryWriteInputBatch(buffer[first..], 2, Sample.AsSpan(2, 3), out int second);

        FrameCodec.TryReadFrame(buffer[..(first + second)], out _, out ReadOnlySpan<byte> firstPayload, out int consumed);
        Assert.Equal(first, consumed);
        Assert.Equal(2, FrameCodec.ReadInputBatch(firstPayload, new InputEvent[8], out long firstTimestamp));
        Assert.Equal(1, firstTimestamp);

        FrameCodec.TryReadFrame(buffer[consumed..(first + second)], out _, out ReadOnlySpan<byte> secondPayload, out _);
        Assert.Equal(3, FrameCodec.ReadInputBatch(secondPayload, new InputEvent[8], out long secondTimestamp));
        Assert.Equal(2, secondTimestamp);
    }

    [Fact]
    public void EncodingAndDecodingAllocateNothing()
    {
        // The wire path carries every event; allocation here would eventually mean gen0
        // collections, and a GC pause reaching the hook thread risks a silent unhook.
        byte[] buffer = new byte[FrameCodec.InputBatchSize(Sample.Length)];
        var decoded = new InputEvent[Sample.Length];

        FrameCodec.TryWriteInputBatch(buffer, 1, Sample, out _);
        FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> warm, out _);
        FrameCodec.ReadInputBatch(warm, decoded, out _);

        long before = GC.GetAllocatedBytesForCurrentThread();

        for (int i = 0; i < 10_000; i++)
        {
            FrameCodec.TryWriteInputBatch(buffer, i, Sample, out _);
            FrameCodec.TryReadFrame(buffer, out _, out ReadOnlySpan<byte> payload, out _);
            FrameCodec.ReadInputBatch(payload, decoded, out _);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }

    [Fact]
    public void AMouseMoveCostsTwelveBytesInABatch()
    {
        // Guards the wire size against drifting upward unnoticed: at kilohertz rates a few extra
        // bytes per event is real bandwidth.
        int one = FrameCodec.InputBatchSize(1);
        int two = FrameCodec.InputBatchSize(2);

        Assert.Equal(FrameCodec.EncodedEventSize, two - one);
        Assert.Equal(12, two - one);
    }
}
