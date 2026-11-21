// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Tests;

public class TestEnvTests
{
    [Theory]
    [InlineData("e2ereal-7ef397a4f7f2", true)]
    [InlineData("agree-032fc91001a7", true)]
    [InlineData("venv", false)]
    [InlineData("venv-whisper", false)]
    [InlineData("e2e-exe", false)]
    [InlineData("e2e-7EF397A4F7F2", false)]
    public void Only_NewDir_folders_are_swept(string name, bool swept) => Assert.Equal(swept, TestEnv.IsTestDirName(name));

    [Fact]
    public void Scoped_dir_is_deleted_on_dispose()
    {
        string d;
        using (var s = TestEnv.Scoped("cleanup"))
        {
            d = s.Path;
            File.WriteAllText(Path.Combine(d, "x.txt"), "x");
            Assert.True(TestEnv.IsTestDirName(Path.GetFileName(d)));
        }
        Assert.False(Directory.Exists(d));
    }
}
