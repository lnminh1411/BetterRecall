// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Views;

/// <summary>Shared data templates. A class-backed dictionary so the templates can use x:Bind.</summary>
public sealed partial class Templates : Microsoft.UI.Xaml.ResourceDictionary
{
    public Templates() => InitializeComponent();
}
