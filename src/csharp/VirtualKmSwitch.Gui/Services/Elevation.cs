using System.ComponentModel;
using System.Diagnostics;
using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using VirtualKmSwitch.Gui.Ipc;

namespace VirtualKmSwitch.Gui.Services;

/// <summary>
/// Sends the occasional request that needs administrator rights, by asking Windows for them.
///
/// The controller stays unelevated. When the service turns a request down for want of rights, a
/// second copy of this executable is started through the ordinary UAC prompt; it sends that one
/// request to the service with its own, elevated token, hands the answer back and exits. Approving
/// the prompt is the person's consent to that one change, and nothing elevated is left running.
///
/// The request and its answer travel over a private pipe that only an elevated process can open.
/// They are not passed on the command line: a pairing code typed in is a secret, and the code shown
/// comes back only in the answer.
/// </summary>
public static class Elevation
{
    /// <summary>The first argument that makes this executable the elevated helper instead of the UI.</summary>
    public const string HelperArgument = "--elevated-request";

    /// <summary>The ERROR_CANCELLED that ShellExecute reports when the UAC prompt is declined.</summary>
    private const int ErrorCancelled = 1223;

    private static readonly TimeSpan ConnectTimeout = TimeSpan.FromSeconds(10);

    /// <summary>
    /// What the helper will carry: the verbs that change whom this machine trusts. Anything else
    /// needs no elevation, so a request for it here is not one this controller made.
    /// </summary>
    private static readonly HashSet<ControlVerb> AdministrativeVerbs =
    [
        ControlVerb.OpenPairingWindow,
        ControlVerb.PairWithCode,
        ControlVerb.RemovePeer,
    ];

    /// <summary>
    /// Sends a request as this user and, only if the service refuses it for want of administrator
    /// rights, again from an elevated helper after a UAC prompt.
    /// </summary>
    /// <remarks>
    /// The service stays the judge of what needs rights: asking unelevated first means nothing here
    /// has to know, and an elevated controller is never prompted.
    /// </remarks>
    public static async Task<ControlResponse> SendAsync(
        ServiceConnection service,
        ControlVerb verb,
        Dictionary<string, string>? arguments = null,
        CancellationToken cancellationToken = default)
    {
        ControlResponse response = await service.SendAsync(verb, arguments, cancellationToken).ConfigureAwait(false);

        if (!response.NeedsAdministrator || IsElevated())
        {
            return response;
        }

        var request = new ControlRequest { Verb = verb, Arguments = arguments };
        return await SendElevatedAsync(service.PipeName, request, cancellationToken).ConfigureAwait(false);
    }

    private static async Task<ControlResponse> SendElevatedAsync(
        string servicePipe, ControlRequest request, CancellationToken cancellationToken)
    {
        string? executable = Environment.ProcessPath;
        if (executable is null)
        {
            return ControlResponse.Fail("the controller could not find its own executable to ask for administrator rights.");
        }

        // Unguessable, and created before the helper exists, so nobody can be listening under this
        // name first and hand the helper a request of their own.
        string channel = $"VirtualKmSwitch.Elevated.{Guid.NewGuid():N}";
        await using NamedPipeServerStream server = CreateChannel(channel);

        Process? helper;
        try
        {
            helper = Process.Start(new ProcessStartInfo
            {
                FileName = executable,
                Arguments = $"{HelperArgument} {channel} \"{servicePipe}\"",
                UseShellExecute = true,
                Verb = "runas",
            });
        }
        catch (Win32Exception ex) when (ex.NativeErrorCode == ErrorCancelled)
        {
            return ControlResponse.Fail("administrator approval was declined, so nothing was changed.");
        }

        if (helper is null)
        {
            return ControlResponse.Fail("the elevated helper did not start.");
        }

        using (helper)
        {
            using var abandon = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            Task connecting = server.WaitForConnectionAsync(abandon.Token);
            Task exiting = helper.WaitForExitAsync(abandon.Token);

            Task first = await Task.WhenAny(connecting, exiting).ConfigureAwait(false);
            await abandon.CancelAsync().ConfigureAwait(false);

            if (first != connecting)
            {
                return ControlResponse.Fail("the elevated helper exited before it could send the request.");
            }

            await connecting.ConfigureAwait(false);

            await ControlProtocol.WriteAsync(server, request, cancellationToken).ConfigureAwait(false);
            return await ControlProtocol.ReadAsync<ControlResponse>(server, cancellationToken).ConfigureAwait(false)
                ?? ControlResponse.Fail("the elevated helper exited without an answer from the service.");
        }
    }

    /// <summary>
    /// The private pipe to the helper. Administrators only: an elevated process has that group
    /// enabled and this unelevated one does not, so another program running as this user cannot
    /// connect in the helper's place and feed back an answer of its own.
    /// </summary>
    private static NamedPipeServerStream CreateChannel(string name)
    {
        var security = new PipeSecurity();
        security.AddAccessRule(new PipeAccessRule(
            new SecurityIdentifier(WellKnownSidType.BuiltinAdministratorsSid, null),
            PipeAccessRights.FullControl,
            AccessControlType.Allow));

        return NamedPipeServerStreamAcl.Create(
            name,
            PipeDirection.InOut,
            maxNumberOfServerInstances: 1,
            PipeTransmissionMode.Byte,
            PipeOptions.Asynchronous,
            inBufferSize: 0,
            outBufferSize: 0,
            security);
    }

    private static bool IsElevated()
    {
        using WindowsIdentity identity = WindowsIdentity.GetCurrent();
        return new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
    }

    /// <summary>
    /// The helper: no window, one request. Reads it from the controller's private pipe, sends it to
    /// the service with this process's elevated token, writes the answer back.
    /// </summary>
    public static int RunHelper(string channel, string servicePipe) =>
        RunHelperAsync(channel, servicePipe).GetAwaiter().GetResult();

    private static async Task<int> RunHelperAsync(string channel, string servicePipe)
    {
        try
        {
            await using var controller = new NamedPipeClientStream(".", channel, PipeDirection.InOut, PipeOptions.Asynchronous);

            // Bounded, so a controller that has gone away leaves no elevated process waiting on it.
            using (var timeout = new CancellationTokenSource(ConnectTimeout))
            {
                await controller.ConnectAsync(timeout.Token).ConfigureAwait(false);
            }

            ControlRequest? request;
            using (var timeout = new CancellationTokenSource(ConnectTimeout))
            {
                request = await ControlProtocol.ReadAsync<ControlRequest>(controller, timeout.Token).ConfigureAwait(false);
            }

            if (request is null)
            {
                return 1;
            }

            ControlResponse response = AdministrativeVerbs.Contains(request.Verb)
                ? await SendToServiceAsync(servicePipe, request).ConfigureAwait(false)
                : ControlResponse.Fail($"{request.Verb} is not sent with administrator rights.");

            await ControlProtocol.WriteAsync(controller, response, CancellationToken.None).ConfigureAwait(false);
            return 0;
        }
        catch (Exception)
        {
            // Nobody to tell: the controller sees the pipe close and reports that instead.
            return 1;
        }
    }

    private static async Task<ControlResponse> SendToServiceAsync(string servicePipe, ControlRequest request)
    {
        await using var client = new ControlPipeClient(servicePipe);

        try
        {
            await client.ConnectAsync(TimeSpan.FromSeconds(3)).ConfigureAwait(false);
        }
        catch (Exception)
        {
            return ControlResponse.Fail("The service is not reachable.");
        }

        return await client.SendAsync(request).ConfigureAwait(false);
    }
}
