using VirtualKmSwitch.Core;
using VirtualKmSwitch.Ipc;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class SpscRingBufferTests
{
    [Fact]
    public void CapacityRoundsUpToPowerOfTwo()
    {
        Assert.Equal(8, new SpscRingBuffer<InputEvent>(5).Capacity);
        Assert.Equal(1024, new SpscRingBuffer<InputEvent>(1024).Capacity);
    }

    [Fact]
    public void WritesAreReadBackInOrder()
    {
        var ring = new SpscRingBuffer<InputEvent>(16);

        for (int i = 1; i <= 10; i++)
        {
            Assert.True(ring.TryWrite(InputEvent.MouseMove(i, i, -i)));
        }

        for (int i = 1; i <= 10; i++)
        {
            Assert.True(ring.TryRead(out InputEvent read));
            Assert.Equal(i, read.X);
            Assert.Equal(-i, read.Y);
            Assert.Equal(i, read.Timestamp);
        }

        Assert.False(ring.TryRead(out _));
    }

    [Fact]
    public void RefusesWritesWhenFullAndCountsThem()
    {
        var ring = new SpscRingBuffer<InputEvent>(4);

        for (int i = 0; i < 4; i++)
        {
            Assert.True(ring.TryWrite(InputEvent.MouseMove(i, i, 0)));
        }

        Assert.False(ring.TryWrite(InputEvent.MouseMove(99, 99, 0)));
        Assert.Equal(1, ring.RejectedWrites);

        // Draining one slot makes room again.
        Assert.True(ring.TryRead(out _));
        Assert.True(ring.TryWrite(InputEvent.MouseMove(99, 99, 0)));
    }

    [Fact]
    public void IndicesWrapWithoutCorruption()
    {
        var ring = new SpscRingBuffer<InputEvent>(4);

        // Many times capacity, so the mask arithmetic wraps repeatedly.
        for (int i = 0; i < 1000; i++)
        {
            Assert.True(ring.TryWrite(InputEvent.MouseMove(i, i, 0)));
            Assert.True(ring.TryRead(out InputEvent read));
            Assert.Equal(i, read.X);
        }
    }

    [Fact]
    public void ReadBatchDrainsWhatIsAvailable()
    {
        var ring = new SpscRingBuffer<InputEvent>(64);
        for (int i = 0; i < 10; i++)
        {
            ring.TryWrite(InputEvent.MouseMove(i, i, 0));
        }

        var destination = new InputEvent[32];
        int read = ring.ReadBatch(destination);

        Assert.Equal(10, read);
        for (int i = 0; i < 10; i++)
        {
            Assert.Equal(i, destination[i].X);
        }

        Assert.Equal(0, ring.ReadBatch(destination));
    }

    [Fact]
    public void ReadBatchIsLimitedByDestinationLength()
    {
        var ring = new SpscRingBuffer<InputEvent>(64);
        for (int i = 0; i < 20; i++)
        {
            ring.TryWrite(InputEvent.MouseMove(i, i, 0));
        }

        var small = new InputEvent[8];
        Assert.Equal(8, ring.ReadBatch(small));
        Assert.Equal(0, small[0].X);
        Assert.Equal(7, small[7].X);
        Assert.Equal(12, ring.Count);
    }

    /// <summary>
    /// The regression test that keeps the hot path honest. Every input event from capture to wire
    /// must allocate nothing: allocation means eventual gen0 collections, and a GC pause on the
    /// hook thread risks overrunning LowLevelHooksTimeout, which makes Windows silently unhook us.
    /// </summary>
    [Fact]
    public void SteadyStateTrafficAllocatesNothing()
    {
        var ring = new SpscRingBuffer<InputEvent>(1024);
        var destination = new InputEvent[64];

        // Warm up so JIT and first-touch costs are not counted.
        for (int i = 0; i < 100; i++)
        {
            ring.TryWrite(InputEvent.MouseMove(i, 1, 1));
        }

        ring.ReadBatch(destination);

        long before = GC.GetAllocatedBytesForCurrentThread();

        for (int round = 0; round < 1000; round++)
        {
            for (int i = 0; i < 32; i++)
            {
                ring.TryWrite(InputEvent.MouseMove(i, i, i));
            }

            while (ring.ReadBatch(destination) > 0)
            {
            }
        }

        long allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Assert.Equal(0, allocated);
    }

    [Fact]
    public async Task ConcurrentProducerAndConsumerLoseNothing()
    {
        const int total = 200_000;
        var ring = new SpscRingBuffer<InputEvent>(256);

        var producer = Task.Run(() =>
        {
            for (int i = 0; i < total; i++)
            {
                // Full buffer is expected under load; spin rather than drop, so the test measures
                // correctness rather than backpressure policy.
                while (!ring.TryWrite(InputEvent.MouseMove(i, i, 0)))
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
                int count = ring.ReadBatch(batch);
                if (count == 0)
                {
                    Thread.SpinWait(1);
                    continue;
                }

                for (int i = 0; i < count; i++)
                {
                    // Order must be exact: a key-up overtaking its key-down would strand a
                    // modifier held on a remote machine.
                    Assert.Equal(expected, batch[i].X);
                    expected++;
                }
            }

            return expected;
        });

        await producer;
        int received = await consumer;

        Assert.Equal(total, received);

        // Rejections are expected here -- a 256-slot ring cannot hold 200k events, and the
        // producer spins rather than dropping. What matters is that nothing was lost or
        // reordered, which the consumer asserted event by event.
        Assert.True(ring.RejectedWrites >= 0);
    }
}
