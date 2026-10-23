using VirtualKmSwitch.Core;
using VirtualKmSwitch.InputEngine;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class MouseCalibrationTests
{
    private static readonly PointerBallistics Defaults = PointerBallistics.Default;

    private static (int X, int Y) Sum(Span<InputEvent> events, int count)
    {
        int x = 0, y = 0;
        for (int i = 0; i < count; i++)
        {
            x += events[i].X;
            y += events[i].Y;
        }

        return (x, y);
    }

    [Fact]
    public void SpeedSliderMidpointIsUnityAndTheCurveIsNotLinear()
    {
        Assert.Equal(1.0, Defaults.SpeedMultiplier, 5);
        Assert.Equal(0.5, new PointerBallistics(6, 10, 1, 6).SpeedMultiplier, 5);
        Assert.Equal(3.5, new PointerBallistics(6, 10, 1, 20).SpeedMultiplier, 5);

        // Slider position is not proportional to multiplier: 20 is 3.5x, not 2x of 10.
        Assert.NotEqual(2.0, new PointerBallistics(6, 10, 1, 20).SpeedMultiplier, 5);
    }

    [Fact]
    public void NoChunkExceedsTheAccelerationThreshold()
    {
        // This is the whole point: a chunk above the threshold gets silently doubled by Windows.
        var calibration = new MouseCalibration(Defaults);
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 100, -73, output);

        Assert.True(count > 1, "a large delta must be split");
        for (int i = 0; i < count; i++)
        {
            Assert.True(Math.Abs(output[i].X) <= Defaults.Threshold1, $"chunk X {output[i].X} exceeds threshold");
            Assert.True(Math.Abs(output[i].Y) <= Defaults.Threshold1, $"chunk Y {output[i].Y} exceeds threshold");
        }
    }

    [Fact]
    public void ChunksSumToTheRequestedTotal()
    {
        var calibration = new MouseCalibration(Defaults);
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        foreach ((int dx, int dy) in new[] { (100, -73), (7, 0), (0, -1), (-45, 45), (1, 1) })
        {
            calibration.Reset();
            int count = calibration.Prepare(1, dx, dy, output);
            Assert.Equal((dx, dy), Sum(output, count));
        }
    }

    [Fact]
    public void SmallDeltasStayAsOneChunk()
    {
        var calibration = new MouseCalibration(Defaults);
        Span<InputEvent> output = stackalloc InputEvent[16];

        // Under the threshold, no splitting is needed and none should happen.
        Assert.Equal(1, calibration.Prepare(1, 3, 2, output));
    }

    [Fact]
    public void AccelerationDisabledMeansNoChunkingAtAll()
    {
        // With acceleration off there is no doubling to avoid, so a big delta stays whole.
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, Acceleration: 0, Speed: 10));
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 500, 500, output);

        Assert.Equal(1, count);
        Assert.Equal((500, 500), Sum(output, count));
    }

    [Fact]
    public void FastPointerSpeedIsCompensatedByScalingDown()
    {
        // Speed 14 doubles motion, so half as much must be sent to travel the same distance.
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, Speed: 14));
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 100, 0, output);

        Assert.Equal(50, Sum(output, count).X);
    }

    [Fact]
    public void SlowPointerSpeedIsCompensatedByScalingUp()
    {
        // Speed 6 halves motion, so twice as much must be sent.
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, Speed: 6));
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 10, 0, output);

        Assert.Equal(20, Sum(output, count).X);
    }

    [Fact]
    public void SubUnitMovementAccumulatesRatherThanVanishing()
    {
        // At speed 20 a delta of 1 scales to 0.28, which truncates to zero. Without carrying the
        // remainder the pointer would never move at all during slow, precise movement -- a far
        // worse artefact than being a fraction of a pixel out.
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, Speed: 20));
        Span<InputEvent> output = stackalloc InputEvent[16];

        int emitted = 0;
        for (int i = 0; i < 20; i++)
        {
            int count = calibration.Prepare(i, 1, 0, output);
            emitted += Sum(output, count).X;
        }

        // 20 units at 1/3.5 scaling is ~5.7; the carry should have produced most of that.
        Assert.InRange(emitted, 4, 6);
    }

    [Fact]
    public void ResetDiscardsCarry()
    {
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, Speed: 20));
        Span<InputEvent> output = stackalloc InputEvent[16];

        for (int i = 0; i < 3; i++)
        {
            calibration.Prepare(i, 1, 0, output);
        }

        calibration.Reset();

        // With the carry cleared, a single sub-unit delta must produce nothing.
        Assert.Equal(0, calibration.Prepare(9, 1, 0, output));
    }

    [Fact]
    public void ChunkCountIsBoundedForEnormousDeltas()
    {
        var calibration = new MouseCalibration(Defaults);
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 100_000, 0, output);

        Assert.InRange(count, 1, MouseCalibration.MaxChunksPerEvent);
        Assert.Equal(100_000, Sum(output, count).X);
    }

    [Fact]
    public void CorrectionCurveInterpolatesBetweenMeasuredSamples()
    {
        var curve = new MouseCorrectionCurve([10, 100], [1.5, 1.1]);

        Assert.Equal(1.5, curve.FactorFor(10), 4);
        Assert.Equal(1.1, curve.FactorFor(100), 4);
        Assert.Equal(1.3, curve.FactorFor(55), 2);

        // Held flat beyond the measured range rather than extrapolated into nonsense.
        Assert.Equal(1.5, curve.FactorFor(1), 4);
        Assert.Equal(1.1, curve.FactorFor(10_000), 4);
    }

    [Fact]
    public void CorrectionCurveIsAppliedByMagnitudeNotPerAxis()
    {
        // The acceleration curve responds to the overall movement, so a diagonal move of the same
        // magnitude as a horizontal one must be corrected the same way.
        var curve = new MouseCorrectionCurve([10, 100], [2.0, 1.0]);
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, 10)) { Correction = curve };

        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 10, 0, output);
        Assert.Equal(20, Sum(output, count).X);

        // 30/40 has magnitude 50, midway on the curve, so a factor of ~1.5.
        calibration.Reset();
        count = calibration.Prepare(1, 30, 40, output);
        (int x, int y) = Sum(output, count);
        Assert.InRange(x, 43, 47);
        Assert.InRange(y, 58, 62);
    }

    [Fact]
    public void IdentityCurveLeavesMotionUnchanged()
    {
        var calibration = new MouseCalibration(new PointerBallistics(6, 10, 0, 10));
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];

        int count = calibration.Prepare(1, 137, -42, output);
        Assert.Equal((137, -42), Sum(output, count));
    }

    [Fact]
    public void PreparationAllocatesNothing()
    {
        var calibration = new MouseCalibration(Defaults);
        Span<InputEvent> output = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];
        calibration.Prepare(1, 10, 10, output);

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            calibration.Prepare(i, 13, -7, output);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }
}
