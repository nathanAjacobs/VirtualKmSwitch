using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;

namespace VirtualKmSwitch.Gui;

/// <summary>
/// Small modal dialogs: confirm, prompt for text, choose one of several.
///
/// Built in code because they are all the same shape -- a title, a sentence, perhaps an input, and
/// buttons -- and styled entirely by the application's own classes, so they follow the theme like
/// everything else.
/// </summary>
internal static class Dialogs
{
    /// <summary>Asks a yes-or-no question. Returns true for <paramref name="accept"/>.</summary>
    public static async Task<bool> ConfirmAsync(Control anchor, string title, string message, string accept, string cancel)
    {
        Window dialog = Create(title);
        dialog.Content = Layout(
            title,
            message,
            body: null,
            Buttons(dialog, (cancel, false, false), (accept, true, true)));

        return await ShowAsync<bool>(anchor, dialog);
    }

    /// <summary>Asks for a line of text. Returns null if cancelled or left empty.</summary>
    public static async Task<string?> PromptAsync(Control anchor, string title, string message, string? placeholder, int maxLength)
    {
        Window dialog = Create(title);

        var input = new TextBox
        {
            PlaceholderText = placeholder,
            MaxLength = maxLength,
            MinWidth = 320,
        };

        // Enter is "OK", which is how anyone typing a code expects to finish.
        input.KeyDown += (_, e) =>
        {
            if (e.Key == Key.Enter)
            {
                dialog.Close(input.Text);
                e.Handled = true;
            }
        };

        dialog.Content = Layout(
            title,
            message,
            input,
            Buttons(dialog, ("Cancel", null, false), ("OK", input, true)));

        dialog.Opened += (_, _) => input.Focus();

        string? result = await ShowAsync<string?>(anchor, dialog);
        return string.IsNullOrWhiteSpace(result) ? null : result;
    }

    /// <summary>Offers a list of choices. Returns the one picked, or null if cancelled.</summary>
    public static async Task<string?> ChooseAsync(Control anchor, string title, string cancel, IReadOnlyList<string> choices)
    {
        Window dialog = Create(title);

        var list = new StackPanel { Spacing = 4 };
        foreach (string choice in choices)
        {
            var button = new Button
            {
                Content = choice,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Left,
            };

            button.Click += (_, _) => dialog.Close(choice);
            list.Children.Add(button);
        }

        dialog.Content = Layout(title, message: null, list, Buttons(dialog, (cancel, null, false)));

        return await ShowAsync<string?>(anchor, dialog);
    }

    private static Window Create(string title) => new()
    {
        Title = title,
        SizeToContent = SizeToContent.WidthAndHeight,
        CanResize = false,
        ShowInTaskbar = false,
        WindowStartupLocation = WindowStartupLocation.CenterOwner,
        MaxWidth = 520,
    };

    private static Control Layout(string title, string? message, Control? body, Control buttons)
    {
        var panel = new StackPanel { Spacing = 14, Margin = new Thickness(24, 20, 24, 20) };

        panel.Children.Add(new TextBlock { Text = title, Classes = { "strong" } });

        if (!string.IsNullOrEmpty(message))
        {
            panel.Children.Add(new TextBlock { Text = message, TextWrapping = Avalonia.Media.TextWrapping.Wrap, MaxWidth = 460 });
        }

        if (body is not null)
        {
            panel.Children.Add(body);
        }

        panel.Children.Add(buttons);
        return panel;
    }

    /// <summary>
    /// The buttons along the bottom. Each closes the dialog with its result -- or, for a result that
    /// is a text box, with whatever was typed into it.
    /// </summary>
    private static Control Buttons(Window dialog, params (string Text, object? Result, bool IsDefault)[] buttons)
    {
        var row = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 10,
            HorizontalAlignment = HorizontalAlignment.Right,
            Margin = new Thickness(0, 6, 0, 0),
        };

        foreach ((string text, object? result, bool isDefault) in buttons)
        {
            var button = new Button { Content = text, IsDefault = isDefault, IsCancel = !isDefault };

            if (isDefault)
            {
                button.Classes.Add("primary");
            }

            button.Click += (_, _) => dialog.Close(result is TextBox input ? input.Text : result);
            row.Children.Add(button);
        }

        return row;
    }

    /// <summary>
    /// Shows the dialog modally over the window holding <paramref name="anchor"/>. Dialogs are only
    /// ever opened from a click in that window, so it is always there to own them.
    /// </summary>
    private static async Task<T> ShowAsync<T>(Control anchor, Window dialog) =>
        TopLevel.GetTopLevel(anchor) is Window owner
            ? await dialog.ShowDialog<T>(owner)
            : default!;
}
