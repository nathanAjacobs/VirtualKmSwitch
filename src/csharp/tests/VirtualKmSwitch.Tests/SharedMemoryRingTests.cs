using VirtualKmSwitch.Core;
using VirtualKmSwitch.Ipc;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// Exercises the cross-process ring within one process. Both ends map the same named region, so
/// the shared-memory path, the header handshake and the index arithmetic are all real; only the
/// process boundary is absent.
/// </summary>
public class SharedMemoryRingTests
{
    /// <summary>
    /// Named per test so parallel runs cannot collide. Created in the Local namespace without the
    /// SYSTEM-only DACL, because the test process is an ordinary user: Global needs
    /// SeCreateGlobalPrivilege, which only the real service has. The ring logic under test is
    /// identical either way.
    /// </summary>
    private static SharedMemoryRing<InputEvent> CreateRing(string suffix, int capacity = 64) =>
        SharedMemoryRing<InputEvent>.Create(
            $"Test.{suffix}.{Environment.ProcessId}", capacity, systemOnly: false, globalNamespace: false);

    private static SharedMemoryRing<InputEvent> OpenRing(string suffix, int capacity = 64) =>
        SharedMemoryRing<InputEvent>.Open(
            $"Test.{suffix}.{Environment.ProcessId}", capacity, globalNamespace: false);

    [Fact]
    public void ItemsSurviveTheSharedMemoryRoundTrip()
    {
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(ItemsSurviveTheSharedMemoryRoundTrip));
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(ItemsSurviveTheSharedMemoryRoundTrip));

        for (int i = 1; i <= 10; i++)
        {
            Assert.True(writer.TryWrite(InputEvent.MouseMove(i, i, -i)));
        }

        var received = new InputEvent[32];
        int count = reader.ReadBatch(received);

        Assert.Equal(10, count);
        for (int i = 0; i < 10; i++)
        {
            Assert.Equal(i + 1, received[i].X);
            Assert.Equal(-(i + 1), received[i].Y);
            Assert.Equal(i + 1, received[i].Timestamp);
        }
    }

    [Fact]
    public void EveryFieldCrossesTheBoundaryIntact()
    {
        // The struct is copied as raw bytes through shared memory, so a layout or packing mistake
        // would silently corrupt fields rather than fail loudly.
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(EveryFieldCrossesTheBoundaryIntact));
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(EveryFieldCrossesTheBoundaryIntact));

        var original = new InputEvent(
            long.MaxValue,
            InputEventKind.Key,
            InputEventFlags.Extended | InputEventFlags.Up,
            0x1D,
            int.MinValue,
            int.MaxValue);

        writer.TryWrite(original);

        var received = new InputEvent[1];
        Assert.Equal(1, reader.ReadBatch(received));
        Assert.Equal(original, received[0]);
    }

    [Fact]
    public void FullRingRefusesWritesAndCountsThem()
    {
        using SharedMemoryRing<InputEvent> ring = CreateRing(nameof(FullRingRefusesWritesAndCountsThem), 4);

        for (int i = 0; i < ring.Capacity; i++)
        {
            Assert.True(ring.TryWrite(InputEvent.MouseMove(i, i, 0)));
        }

        Assert.False(ring.TryWrite(InputEvent.MouseMove(99, 99, 0)));
        Assert.Equal(1, ring.RejectedWrites);
    }

    [Fact]
    public void IndicesWrapWithoutCorruption()
    {
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(IndicesWrapWithoutCorruption), 4);
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(IndicesWrapWithoutCorruption), 4);

        var received = new InputEvent[4];
        for (int i = 0; i < 1000; i++)
        {
            Assert.True(writer.TryWrite(InputEvent.MouseMove(i, i, 0)));
            Assert.Equal(1, reader.ReadBatch(received));
            Assert.Equal(i, received[0].X);
        }
    }

    [Fact]
    public void MismatchedCapacityIsRejectedRatherThanMisread()
    {
        // Both sides compute slot addresses from the capacity. Disagreeing about it would not
        // fail — it would silently read the wrong memory, so the header is checked on open.
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(MismatchedCapacityIsRejectedRatherThanMisread), 64);

        Assert.Throws<InvalidOperationException>(
            () => OpenRing(nameof(MismatchedCapacityIsRejectedRatherThanMisread), 128));
    }

    [Fact]
    public void DiscardPendingDropsStaleInput()
    {
        // The ring outlives an agent that crashes. Replaying what it had queued would inject
        // keystrokes captured seconds ago into whatever has focus now.
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(DiscardPendingDropsStaleInput));
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(DiscardPendingDropsStaleInput));

        for (int i = 0; i < 10; i++)
        {
            writer.TryWrite(InputEvent.MouseMove(i, i, 0));
        }

        reader.DiscardPending();

        Assert.Equal(0, reader.ReadBatch(new InputEvent[32]));

        // Still usable afterwards.
        writer.TryWrite(InputEvent.MouseMove(99, 5, 5));
        Assert.Equal(1, reader.ReadBatch(new InputEvent[32]));
    }

    [Fact]
    public void SignalWakesAWaitingConsumer()
    {
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(SignalWakesAWaitingConsumer));
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(SignalWakesAWaitingConsumer));

        Assert.False(reader.WaitForData(TimeSpan.FromMilliseconds(50)));

        writer.TryWrite(InputEvent.MouseMove(1, 1, 1));
        writer.SignalDataAvailable();

        Assert.True(reader.WaitForData(TimeSpan.FromSeconds(2)));
    }

    /// <summary>
    /// The assertion the plan asks for at this step. Allocation on this path would eventually mean
    /// gen0 collections, and a GC pause reaching the hook thread risks the LowLevelHooksTimeout,
    /// after which Windows unhooks us silently.
    /// </summary>
    [Fact]
    public void SteadyStateTrafficAllocatesNothing()
    {
        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(SteadyStateTrafficAllocatesNothing), 1024);
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(SteadyStateTrafficAllocatesNothing), 1024);

        var destination = new InputEvent[64];

        // Warm up so JIT and first-touch page faults are not counted.
        for (int i = 0; i < 100; i++)
        {
            writer.TryWrite(InputEvent.MouseMove(i, 1, 1));
        }

        reader.ReadBatch(destination);

        long before = GC.GetAllocatedBytesForCurrentThread();

        for (int round = 0; round < 1000; round++)
        {
            for (int i = 0; i < 32; i++)
            {
                writer.TryWrite(InputEvent.MouseMove(i, i, i));
            }

            while (reader.ReadBatch(destination) > 0)
            {
            }
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }

    [Fact]
    public async Task ConcurrentProducerAndConsumerLoseNothing()
    {
        const int total = 100_000;

        using SharedMemoryRing<InputEvent> writer = CreateRing(nameof(ConcurrentProducerAndConsumerLoseNothing), 256);
        using SharedMemoryRing<InputEvent> reader = OpenRing(nameof(ConcurrentProducerAndConsumerLoseNothing), 256);

        var producer = Task.Run(() =>
        {
            for (int i = 0; i < total; i++)
            {
                while (!writer.TryWrite(InputEvent.MouseMove(i, i, 0)))
                {
                    Thread.SpinWait(1);
                }
            }
        });

        var consumer = Task.Run(() =>
        {
            var batch = new InputEvent[64];
            int expected = 0;

            while (expected < total)
            {
                int count = reader.ReadBatch(batch);
                if (count == 0)
                {
                    Thread.SpinWait(1);
                    continue;
                }

                for (int i = 0; i < count; i++)
                {
                    // Order must hold exactly: a key-up overtaking its key-down would strand a
                    // modifier held on a remote machine.
                    Assert.Equal(expected, batch[i].X);
                    expected++;
                }
            }

            return expected;
        });

        await producer;
        Assert.Equal(total, await consumer);
    }
}
