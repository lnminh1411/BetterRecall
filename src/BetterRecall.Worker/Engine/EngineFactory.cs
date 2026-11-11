// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Hosting;

namespace BetterRecall.Worker.Engine;

public static class EngineFactory
{
    public static IWorkerEngine Create(WorkerSettings settings, string home)
        => settings.Fake ? new FakeEngine() : new RealEngine(settings, home);
}
