using System.Diagnostics;
using System.Net;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography.X509Certificates;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// The enrollment exchange over real TLS, which is the only place the join key is ever used and
/// the only moment an unenrolled machine is allowed to talk to this one.
/// </summary>
public class PairingWindowTests : IDisposable
{
    private const string Key = "ABCD-EFGH-JKLM-NPQR";

    private readonly X509Certificate2 _accepterCertificate =
        NodeCertificate.Create("CN=PairAccepter", StoreLocation.CurrentUser);

    private readonly X509Certificate2 _joinerCertificate =
        NodeCertificate.Create("CN=PairJoiner", StoreLocation.CurrentUser);

    private readonly Guid _accepterId = Guid.NewGuid();
    private readonly Guid _joinerId = Guid.NewGuid();

    private PairingWindow OpenWindow(TimeSpan? duration = null) =>
        new(
            new ClusterIdentity
            {
                NodeId = _accepterId,
                ClusterId = "0123456789ABCDEF",
                ClusterName = "Home",
                JoinKey = JoinKey.Normalize(Key),
                DisplayName = "Desk",
            },
            _accepterCertificate,
            duration ?? TimeSpan.FromSeconds(30),
            port: 0);

    private Task<EnrollmentOutcome> JoinAsync(int port, string key = Key) =>
        PairingWindow.JoinAsync(
            "127.0.0.1", port, key, _joinerId, _joinerCertificate, "Laptop");

    [Fact]
    public async Task TwoMachinesHoldingTheSameKeyEnrollEachOther()
    {
        await using PairingWindow window = OpenWindow();

        var accepted = new TaskCompletionSource<EnrollmentOutcome>();
        window.AttemptCompleted += outcome => accepted.TrySetResult(outcome);

        EnrollmentOutcome joinerSaw = await JoinAsync(window.Port);
        EnrollmentOutcome accepterSaw = await accepted.Task.WaitAsync(TimeSpan.FromSeconds(15));

        Assert.True(joinerSaw.Accepted, joinerSaw.Detail);
        Assert.True(accepterSaw.Accepted, accepterSaw.Detail);

        // Each side comes away with the other's identity and the exact certificate it will pin.
        // Everything after this point is authorised by that thumbprint alone.
        Assert.Equal(_accepterId, joinerSaw.PeerNodeId);
        Assert.Equal(NodeCertificate.ThumbprintOf(_accepterCertificate), joinerSaw.PeerThumbprint);
        Assert.Equal("Desk", joinerSaw.PeerDisplayName);
        Assert.Equal("Home", joinerSaw.ClusterName);

        Assert.Equal(_joinerId, accepterSaw.PeerNodeId);
        Assert.Equal(NodeCertificate.ThumbprintOf(_joinerCertificate), accepterSaw.PeerThumbprint);
        Assert.Equal("Laptop", accepterSaw.PeerDisplayName);
    }

    [Fact]
    public async Task AWrongKeyIsRefused()
    {
        await using PairingWindow window = OpenWindow();

        EnrollmentOutcome outcome = await JoinAsync(window.Port, "ZZZZ-ZZZZ-ZZZZ-ZZZZ");

        Assert.False(outcome.Accepted);
        Assert.Equal(EnrollmentResult.WrongKey, outcome.Result);
    }

    [Fact]
    public async Task TranscriptionDifferencesInTheTypedKeyStillJoin()
    {
        // The key is read off one screen and typed into another, so the spacing and case it
        // arrives in must not be the difference between joining and being told it is wrong.
        await using PairingWindow window = OpenWindow();

        EnrollmentOutcome outcome = await JoinAsync(window.Port, "  abcd efgh JKLM-npqr ");

        Assert.True(outcome.Accepted, outcome.Detail);
    }

    [Fact]
    public async Task AnInterceptedConnectionIsRefusedEvenWithTheRightKey()
    {
        // The property the whole thumbprint-binding exists for. The relay knows neither private
        // key; it only re-terminates TLS, which is exactly what an attacker on the same network
        // can do. It can relay both proofs verbatim and they still do not verify, because each was
        // computed over the certificates the honest peers actually presented.
        await using PairingWindow window = OpenWindow();
        await using var relay = new InterceptingRelay(window.Port);

        EnrollmentOutcome outcome = await JoinAsync(relay.Port);

        Assert.False(outcome.Accepted);

        // And it is named for what it is. Reporting "wrong key" here would send the user to check
        // the key while the interceptor waits for them to try again.
        Assert.Equal(EnrollmentResult.CertificateMismatch, outcome.Result);
        Assert.Contains("not direct", outcome.Detail);
    }

    [Fact]
    public async Task AClosedWindowAcceptsNothingAtAll()
    {
        int port;

        await using (PairingWindow window = OpenWindow())
        {
            port = window.Port;
        }

        // Not refused by a check inside the handshake -- there is nothing listening. The join key
        // is unreachable rather than merely unused while the window is shut.
        await Assert.ThrowsAnyAsync<SocketException>(() => JoinAsync(port));
    }

    [Fact]
    public async Task TheWindowClosesItselfWhenItsTimeIsUp()
    {
        await using PairingWindow window = OpenWindow(TimeSpan.FromMilliseconds(300));

        Assert.True(window.IsOpen);
        await WaitForAsync(() => !window.IsOpen, "the window to close on its own");
    }

    [Fact]
    public async Task ARunOfAttemptsClosesTheWindowRatherThanThrottlingIt()
    {
        await using PairingWindow window = OpenWindow();

        // Eighty bits of key is not something a person fails at eleven times in a row. Being
        // probed is the only explanation, and the answer to it is to stop being reachable.
        for (int attempt = 0; attempt < 12 && window.IsOpen; attempt++)
        {
            try
            {
                using var probe = new TcpClient();
                await probe.ConnectAsync("127.0.0.1", window.Port);
            }
            catch (SocketException)
            {
                break;
            }

            await Task.Delay(20);
        }

        await WaitForAsync(() => !window.IsOpen, "the window to close after repeated attempts");
    }

    private static async Task WaitForAsync(Func<bool> condition, string what)
    {
        var deadline = Stopwatch.StartNew();
        while (deadline.Elapsed < TimeSpan.FromSeconds(15))
        {
            if (condition())
            {
                return;
            }

            await Task.Delay(10);
        }

        Assert.Fail("timed out waiting for " + what);
    }

    /// <summary>
    /// A machine-in-the-middle: terminates TLS on both sides with its own certificate and copies
    /// the plaintext between them without altering it.
    /// </summary>
    private sealed class InterceptingRelay : IAsyncDisposable
    {
        private readonly X509Certificate2 _certificate =
            NodeCertificate.Create("CN=Interceptor", StoreLocation.CurrentUser);

        private readonly TcpListener _listener;
        private readonly CancellationTokenSource _stopping = new();
        private readonly Task _loop;
        private readonly int _targetPort;

        public InterceptingRelay(int targetPort)
        {
            _targetPort = targetPort;
            _listener = new TcpListener(IPAddress.Loopback, 0);
            _listener.Start();
            Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
            _loop = Task.Run(() => RunAsync(_stopping.Token));
        }

        public int Port { get; }

        private async Task RunAsync(CancellationToken cancellationToken)
        {
            try
            {
                using TcpClient victim = await _listener.AcceptTcpClientAsync(cancellationToken);
                using var upstream = new TcpClient();
                await upstream.ConnectAsync("127.0.0.1", _targetPort, cancellationToken);

                await using var toVictim = new SslStream(
                    victim.GetStream(), false, (_, _, _, _) => true);

                await using var toAccepter = new SslStream(
                    upstream.GetStream(), false, (_, _, _, _) => true);

                Task asServer = toVictim.AuthenticateAsServerAsync(
                    new SslServerAuthenticationOptions
                    {
                        ServerCertificate = _certificate,
                        ClientCertificateRequired = true,
                        EnabledSslProtocols = SslProtocols.Tls13,
                        CertificateRevocationCheckMode = X509RevocationMode.NoCheck,
                    },
                    cancellationToken);

                Task asClient = toAccepter.AuthenticateAsClientAsync(
                    new SslClientAuthenticationOptions
                    {
                        TargetHost = "VirtualKmSwitch",
                        ClientCertificates = [_certificate],
                        EnabledSslProtocols = SslProtocols.Tls13,
                        CertificateRevocationCheckMode = X509RevocationMode.NoCheck,
                    },
                    cancellationToken);

                await Task.WhenAll(asServer, asClient);

                await Task.WhenAny(
                    Pump(toVictim, toAccepter, cancellationToken),
                    Pump(toAccepter, toVictim, cancellationToken));
            }
            catch (Exception)
            {
                // The victim aborting mid-exchange is the expected end of this, not a fault.
            }
        }

        private static async Task Pump(SslStream from, SslStream to, CancellationToken cancellationToken)
        {
            byte[] buffer = new byte[8192];

            while (!cancellationToken.IsCancellationRequested)
            {
                int read = await from.ReadAsync(buffer, cancellationToken);
                if (read == 0)
                {
                    return;
                }

                await to.WriteAsync(buffer.AsMemory(0, read), cancellationToken);
            }
        }

        public async ValueTask DisposeAsync()
        {
            await _stopping.CancelAsync();
            _listener.Stop();
            await Task.WhenAny(_loop, Task.Delay(TimeSpan.FromSeconds(2)));
            _stopping.Dispose();
            _certificate.Dispose();
        }
    }

    public void Dispose()
    {
        GC.SuppressFinalize(this);
        _accepterCertificate.Dispose();
        _joinerCertificate.Dispose();
    }
}
