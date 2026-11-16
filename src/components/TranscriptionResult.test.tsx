import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import TranscriptionResult from "@/components/TranscriptionResult";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) => {
      if (key === "result.title") return "Result";
      if (key === "result.stats") {
        return `${vars?.chars} chars · ${vars?.duration}s · ${vars?.cpm} chars/min`;
      }
      if (key === "result.latency.asr") return `ASR ${vars?.ms}ms`;
      if (key === "result.latency.ai") return `AI ${vars?.ms}ms`;
      if (key === "result.latency.total") return `total ${vars?.ms}ms`;
      return key;
    },
  }),
}));

describe("TranscriptionResult", () => {
  it("shows recognition activity over an existing result on subsequent recordings", () => {
    const { container, rerender } = render(
      <TranscriptionResult text="previous result" originalText="previous result" isProcessing copiedId={null} onCopy={vi.fn()} durationSec={1} charCount={15} />,
    );
    expect(container.querySelector(".result-card")).toHaveAttribute("aria-busy", "true");
    expect(screen.getByText("result.recognizingSpeech")).toBeInTheDocument();
    expect(screen.getByLabelText("result.editableTranscription")).toHaveValue("previous result");
    rerender(<TranscriptionResult text="next result" originalText="next result" isProcessing={false} copiedId={null} onCopy={vi.fn()} durationSec={1} charCount={11} />);
    expect(container.querySelector(".result-card")).toHaveAttribute("aria-busy", "false");
    expect(screen.queryByText("result.recognizingSpeech")).not.toBeInTheDocument();
    expect(screen.getByLabelText("result.editableTranscription")).toHaveValue("next result");
  });
  it("shows ASR, AI polish, and total latency when timing is available", () => {
    render(
      <TranscriptionResult
        text="hello"
        originalText="hello"
        isProcessing={false}
        copiedId={null}
        onCopy={vi.fn()}
        durationSec={1.2}
        charCount={5}
        timing={{ asrMs: 42, polishMs: 900, totalMs: 948 }}
      />,
    );

    expect(screen.getByText(/ASR 42ms/)).toBeInTheDocument();
    expect(screen.getByText(/AI 900ms/)).toBeInTheDocument();
    expect(screen.getByText(/total 948ms/)).toBeInTheDocument();
  });

  it("auto-grows the result body to fit long wrapped content", () => {
    const originalDescriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight");
    let fakeScrollHeight = 180;
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get: () => fakeScrollHeight,
    });

    try {
      const { rerender } = render(
        <TranscriptionResult
          text="a very long single line that wraps over and over"
          originalText="a very long single line that wraps over and over"
          isProcessing={false}
          copiedId={null}
          onCopy={vi.fn()}
          durationSec={null}
          charCount={null}
        />,
      );
      const body = screen.getByLabelText("result.editableTranscription") as HTMLTextAreaElement;
      expect(body.style.height).toBe("180px");

      fakeScrollHeight = 320;
      rerender(
        <TranscriptionResult
          text="an even longer single line that wraps over and over and over"
          originalText="an even longer single line that wraps over and over and over"
          isProcessing={false}
          copiedId={null}
          onCopy={vi.fn()}
          durationSec={null}
          charCount={null}
        />,
      );
      expect(body.style.height).toBe("320px");
    } finally {
      if (originalDescriptor) {
        Object.defineProperty(HTMLElement.prototype, "scrollHeight", originalDescriptor);
      } else {
        Reflect.deleteProperty(HTMLElement.prototype, "scrollHeight");
      }
    }
  });

  it("collapses long results behind a toggle and expands on demand", () => {
    const scrollHeightDescriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight");
    const clientHeightDescriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight");
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", { configurable: true, get: () => 600 });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => 380 });

    try {
      render(
        <TranscriptionResult
          text={"long transcript ".repeat(80)}
          originalText={"long transcript ".repeat(80)}
          isProcessing={false}
          copiedId={null}
          onCopy={vi.fn()}
          durationSec={null}
          charCount={null}
        />,
      );

      const body = screen.getByLabelText("result.editableTranscription");
      const toggle = screen.getByRole("button", { name: "result.expandAll" });
      expect(body).not.toHaveClass("result-card-body-expanded");
      expect(toggle).toHaveAttribute("aria-expanded", "false");

      fireEvent.click(toggle);

      expect(body).toHaveClass("result-card-body-expanded");
      expect(screen.getByRole("button", { name: "result.collapse" })).toHaveAttribute("aria-expanded", "true");

      fireEvent.click(screen.getByRole("button", { name: "result.collapse" }));
      expect(body).not.toHaveClass("result-card-body-expanded");
    } finally {
      if (scrollHeightDescriptor) Object.defineProperty(HTMLElement.prototype, "scrollHeight", scrollHeightDescriptor);
      if (clientHeightDescriptor) Object.defineProperty(HTMLElement.prototype, "clientHeight", clientHeightDescriptor);
    }
  });

  it("keeps the final result editable and reports corrections on blur", () => {
    const onTextChange = vi.fn();
    render(
      <TranscriptionResult
        text="original text"
        originalText="original text"
        isProcessing={false}
        copiedId={null}
        onCopy={vi.fn()}
        onTextChange={onTextChange}
        durationSec={null}
        charCount={null}
      />,
    );

    const result = screen.getByLabelText("result.editableTranscription");
    expect(result).not.toHaveAttribute("readonly");
    fireEvent.change(result, { target: { value: "corrected text" } });
    fireEvent.blur(result);
    expect(onTextChange).toHaveBeenCalledWith("corrected text");
  });
});
