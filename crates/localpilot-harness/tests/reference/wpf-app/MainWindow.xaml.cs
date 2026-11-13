using System.Windows;

namespace Notes;

public partial class MainWindow : Window
{
    private int saves;

    public MainWindow()
    {
        InitializeComponent();
    }

    private void OnSave(object sender, RoutedEventArgs e)
    {
        saves++;
        Status.Text = $"Saved {saves}";
    }
}
