import { useEffect, useState } from "react";
import { ArrowRight, RotateCcw, Sparkles } from "lucide-react";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";

import {
  getNoteSummaryPromptDefaults,
  setNoteSummaryPartPrompt,
  setNoteSummaryPrompt,
} from "@/api/tauri";
import { SettingsDisclosure } from "@/components/settings/SettingsReveal";
import { useDebouncedCallback } from "@/hooks/useDebouncedCallback";
import type { NoteSummaryPromptDefaults, UserProfile } from "@/types";

/** 只依赖这两条提示词，不把整个画像塞给组件。 */
type NoteSummaryProfile = Pick<
  UserProfile,
  "note_summary_prompt" | "note_summary_part_prompt"
>;

interface NoteSummarySettingsSectionProps {
  profile: NoteSummaryProfile | null;
  /** 跳到「AI 润色」分区配置模型；不传就不显示跳转按钮（单测里通常不传）。 */
  onOpenModelSettings?: () => void;
}

/**
 * 笔记 AI 摘要的提示词设置。
 *
 * 交互原则：**输入框里始终是「实际生效的提示词」**。用户没改过时铺的是内置
 * 默认文案；原样保存不会落库（与默认相同视为「跟随内置」），这样以后升级
 * 默认提示词时老用户也能自动拿到。改过之后保存的是用户自己的版本。
 *
 * 默认文案与长度上限都从后端取（`get_note_summary_prompt_defaults`），
 * 前端不抄一份，避免两处漂移。
 */
export default function NoteSummarySettingsSection({
  profile,
  onOpenModelSettings,
}: NoteSummarySettingsSectionProps) {
  const { t } = useTranslation();
  const [defaults, setDefaults] = useState<NoteSummaryPromptDefaults | null>(null);
  const [mergePrompt, setMergePrompt] = useState("");
  const [partPrompt, setPartPrompt] = useState("");

  // 默认值先到、或 profile 后到，都要能正确落位：分别用两个 effect，
  // 用户配置非空时以配置为准，为空时才铺默认文案。
  useEffect(() => {
    let active = true;
    // 用 async 包一层：即使这个命令在当前环境不可用（单测里只 mock 了部分
    // API），也只降级成「没有恢复默认」，不把整个设置页打挂。
    const loadDefaults = async () => {
      try {
        const value = await getNoteSummaryPromptDefaults();
        if (active) setDefaults(value);
      } catch {
        // 拿不到默认值不影响手写提示词，只是少了「恢复默认」。
      }
    };
    void loadDefaults();
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    setMergePrompt(profile?.note_summary_prompt ?? "");
    setPartPrompt(profile?.note_summary_part_prompt ?? "");
  }, [profile?.note_summary_prompt, profile?.note_summary_part_prompt]);

  useEffect(() => {
    if (!defaults) return;
    // 只在用户还没配置时铺默认：`prev === ""` 说明没配过。
    setMergePrompt((prev) => (prev ? prev : defaults.merge));
    setPartPrompt((prev) => (prev ? prev : defaults.part));
  }, [defaults]);

  const maxChars = defaults?.maxChars ?? 0;
  const mergeTooLong = maxChars > 0 && mergePrompt.length > maxChars;
  const partTooLong = maxChars > 0 && partPrompt.length > maxChars;

  const saveMerge = useDebouncedCallback((value: string) => {
    const trimmed = value.trim();
    // 与内置默认一致就不落库：跟随默认，默认升级时自动生效。
    const useBuiltin = !trimmed || (defaults ? trimmed === defaults.merge.trim() : false);
    setNoteSummaryPrompt(useBuiltin ? null : trimmed).catch(() => {
      toast.error(t("settings.noteSummaryPromptSaveFailed"));
    });
  }, 800, { onUnmount: "flush" });

  const savePart = useDebouncedCallback((value: string) => {
    const trimmed = value.trim();
    const useBuiltin = !trimmed || (defaults ? trimmed === defaults.part.trim() : false);
    setNoteSummaryPartPrompt(useBuiltin ? null : trimmed).catch(() => {
      toast.error(t("settings.noteSummaryPromptSaveFailed"));
    });
  }, 800, { onUnmount: "flush" });

  const handleMergeChange = (value: string) => {
    setMergePrompt(value);
    // 超长就先不落库，等用户删到限制内再存，避免每敲一个字弹一次错。
    // 用新值判断（不能读渲染期的 `mergeTooLong`，那是上一次的）。
    if (maxChars <= 0 || value.length <= maxChars) saveMerge.schedule(value);
  };

  const handlePartChange = (value: string) => {
    setPartPrompt(value);
    if (maxChars <= 0 || value.length <= maxChars) savePart.schedule(value);
  };

  const resetMerge = () => {
    if (!defaults) return;
    setMergePrompt(defaults.merge);
    saveMerge.schedule(defaults.merge);
  };

  const resetPart = () => {
    if (!defaults) return;
    setPartPrompt(defaults.part);
    savePart.schedule(defaults.part);
  };

  return (
    <section className="settings-card" data-nav-id="note-summary">
      <div className="settings-section-header">
        <Sparkles size={15} className="icon-accent" />
        <h2 className="settings-section-title">{t("settings.noteSummary")}</h2>
      </div>
      <div className="settings-column" style={{ gap: 14 }}>
        {/* 两句说明是一组：先说这个分区管什么，再说模型去哪配。 */}
        <div className="settings-column" style={{ gap: 4 }}>
          <p className="settings-hint settings-hint-flush">{t("settings.noteSummaryHint")}</p>
          <div className="settings-prompt-foot" style={{ alignItems: "flex-start" }}>
            <p className="settings-hint settings-hint-flush">
              {t("settings.noteSummaryModelHint")}
            </p>
            {onOpenModelSettings && (
              <button
                type="button"
                className="settings-inline-btn"
                onClick={onOpenModelSettings}
              >
                {t("settings.noteSummaryModelLink")}
                <ArrowRight size={11} />
              </button>
            )}
          </div>
        </div>

        <SettingsDisclosure label={t("settings.noteSummaryPrompt")}>
          <div className="settings-column" style={{ gap: 6 }}>
            <textarea
              className="settings-input"
              aria-label={t("settings.noteSummaryPrompt")}
              value={mergePrompt}
              onChange={(event) => handleMergeChange(event.target.value)}
              rows={9}
              spellCheck={false}
              style={{ resize: "vertical", minHeight: 160, fontFamily: "inherit" }}
            />
            <div className="settings-prompt-foot">
              <span className="settings-hint" data-error={mergeTooLong}>
                {mergeTooLong
                  ? t("settings.noteSummaryPromptTooLong", { limit: maxChars })
                  : t("settings.noteSummaryPromptCount", {
                      used: mergePrompt.length,
                      limit: maxChars,
                    })}
              </span>
              <button
                type="button"
                className="settings-inline-btn"
                disabled={!defaults}
                onClick={resetMerge}
              >
                <RotateCcw size={11} />
                {t("settings.noteSummaryPromptReset")}
              </button>
            </div>
            <p className="settings-hint settings-hint-flush">
              {t("settings.noteSummaryPromptHint")}
            </p>
          </div>
        </SettingsDisclosure>

        <SettingsDisclosure label={t("settings.noteSummaryPartPrompt")}>
          <div className="settings-column" style={{ gap: 6 }}>
            <p className="settings-hint settings-hint-flush">
              {t("settings.noteSummaryPartPromptHint")}
            </p>
            <textarea
              className="settings-input"
              aria-label={t("settings.noteSummaryPartPrompt")}
              value={partPrompt}
              onChange={(event) => handlePartChange(event.target.value)}
              rows={7}
              spellCheck={false}
              style={{ resize: "vertical", minHeight: 130, fontFamily: "inherit" }}
            />
            <div className="settings-prompt-foot">
              <span className="settings-hint" data-error={partTooLong}>
                {partTooLong
                  ? t("settings.noteSummaryPromptTooLong", { limit: maxChars })
                  : t("settings.noteSummaryPromptCount", {
                      used: partPrompt.length,
                      limit: maxChars,
                    })}
              </span>
              <button
                type="button"
                className="settings-inline-btn"
                disabled={!defaults}
                onClick={resetPart}
              >
                <RotateCcw size={11} />
                {t("settings.noteSummaryPromptReset")}
              </button>
            </div>
          </div>
        </SettingsDisclosure>
      </div>
    </section>
  );
}
