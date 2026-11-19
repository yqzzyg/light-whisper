import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  getNoteSummaryPromptDefaults: vi.fn(),
  setNoteSummaryPrompt: vi.fn(),
  setNoteSummaryPartPrompt: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

const toast = vi.hoisted(() => ({ error: vi.fn(), success: vi.fn() }));
vi.mock("sonner", () => ({ toast }));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import NoteSummarySettingsSection from "@/components/settings/NoteSummarySettingsSection";

const DEFAULTS = {
  merge: "内置成稿提示词",
  part: "内置分段提示词 {index}/{total}",
  // 要比两条内置文案都长，否则默认值自己就会触发「超长」。
  maxChars: 200,
};

/** 两个提示词都收在折叠面板里，测试一律先展开。 */
function expand() {
  fireEvent.click(screen.getByText("settings.noteSummaryPrompt"));
  fireEvent.click(screen.getByText("settings.noteSummaryPartPrompt"));
}

beforeEach(() => {
  api.getNoteSummaryPromptDefaults.mockReset();
  api.setNoteSummaryPrompt.mockReset();
  api.setNoteSummaryPartPrompt.mockReset();
  api.getNoteSummaryPromptDefaults.mockResolvedValue(DEFAULTS);
  api.setNoteSummaryPrompt.mockResolvedValue(undefined);
  api.setNoteSummaryPartPrompt.mockResolvedValue(undefined);
  toast.error.mockReset();
});

describe("NoteSummarySettingsSection", () => {
  it("shows the effective builtin prompt when nothing is customized", async () => {
    render(<NoteSummarySettingsSection profile={null} />);
    expand();

    // 输入框里必须是「实际生效的内容」，而不是空白。
    await waitFor(() =>
      expect(screen.getByLabelText("settings.noteSummaryPrompt")).toHaveValue(
        DEFAULTS.merge,
      ),
    );
    expect(screen.getByLabelText("settings.noteSummaryPartPrompt")).toHaveValue(
      DEFAULTS.part,
    );
  });

  it("keeps a saved custom prompt instead of overwriting it with the default", async () => {
    render(
      <NoteSummarySettingsSection
        profile={{
          note_summary_prompt: "我的提纲",
          note_summary_part_prompt: "我的分段要求",
        }}
      />,
    );
    expand();

    await waitFor(() =>
      expect(screen.getByLabelText("settings.noteSummaryPrompt")).toHaveValue(
        "我的提纲",
      ),
    );
    expect(screen.getByLabelText("settings.noteSummaryPartPrompt")).toHaveValue(
      "我的分段要求",
    );
  });

  it("saves a custom prompt", async () => {
    render(<NoteSummarySettingsSection profile={null} />);
    expand();
    const box = await screen.findByLabelText("settings.noteSummaryPrompt");

    fireEvent.change(box, { target: { value: "用周报格式输出" } });

    await waitFor(
      () => expect(api.setNoteSummaryPrompt).toHaveBeenCalledWith("用周报格式输出"),
      { timeout: 2_000 },
    );
  });

  it("stores null when the text matches the builtin default", async () => {
    render(<NoteSummarySettingsSection profile={null} />);
    expand();
    const box = await screen.findByLabelText("settings.noteSummaryPrompt");

    // 原样保存默认文案不该落库，否则以后升级默认提示词时老用户跟不动。
    fireEvent.change(box, { target: { value: DEFAULTS.merge } });

    await waitFor(() => expect(api.setNoteSummaryPrompt).toHaveBeenCalledWith(null), {
      timeout: 2_000,
    });
  });

  it("refuses to save an overlong prompt and says why", async () => {
    render(<NoteSummarySettingsSection profile={null} />);
    expand();
    const box = await screen.findByLabelText("settings.noteSummaryPrompt");
    // 等默认值到达（maxChars 跟着它下发），否则长度判断还没生效。
    await waitFor(() => expect(box).toHaveValue(DEFAULTS.merge));

    fireEvent.change(box, { target: { value: "字".repeat(DEFAULTS.maxChars + 1) } });

    expect(
      await screen.findByText("settings.noteSummaryPromptTooLong"),
    ).toBeInTheDocument();
    // 等过一个防抖窗口，确认超长内容压根没发出去。
    await new Promise((resolve) => setTimeout(resolve, 900));
    expect(api.setNoteSummaryPrompt).not.toHaveBeenCalled();
  });

  it("restores the builtin prompt through the reset button", async () => {
    render(
      <NoteSummarySettingsSection profile={{ note_summary_prompt: "我改过的" }} />,
    );
    expand();
    await waitFor(() =>
      expect(screen.getByLabelText("settings.noteSummaryPrompt")).toHaveValue(
        "我改过的",
      ),
    );

    fireEvent.click(screen.getAllByText("settings.noteSummaryPromptReset")[0]);

    expect(screen.getByLabelText("settings.noteSummaryPrompt")).toHaveValue(
      DEFAULTS.merge,
    );
    await waitFor(() => expect(api.setNoteSummaryPrompt).toHaveBeenCalledWith(null), {
      timeout: 2_000,
    });
  });

  it("says where the model is configured and jumps there on click", () => {
    const onOpenModelSettings = vi.fn();
    render(
      <NoteSummarySettingsSection profile={null} onOpenModelSettings={onOpenModelSettings} />,
    );

    // 摘要吃的是 AI 润色分区的模型，这儿必须说清楚，否则用户找不到。
    expect(screen.getByText("settings.noteSummaryModelHint")).toBeInTheDocument();

    fireEvent.click(screen.getByText("settings.noteSummaryModelLink"));
    expect(onOpenModelSettings).toHaveBeenCalledTimes(1);
  });

  it("hides the shortcut when no jump target is given", () => {
    render(<NoteSummarySettingsSection profile={null} />);

    // 说明文字照常显示，只是没有跳转按钮（没有设置页上下文时）。
    expect(screen.getByText("settings.noteSummaryModelHint")).toBeInTheDocument();
    expect(screen.queryByText("settings.noteSummaryModelLink")).not.toBeInTheDocument();
  });
});
