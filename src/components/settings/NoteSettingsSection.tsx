import { useEffect, useState } from "react";
import { Users } from "lucide-react";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";

import { setNoteDiarizationEnabled } from "@/api/tauri";
import { DEFAULT_NOTE_SETTINGS, type NoteSettings, type UserProfile } from "@/types";

/**
 * 笔记转写的说话人分离开关。
 *
 * 默认开：面对面会议里「谁说的」比逐字稿本身更有用，多数人装完就想看到它。
 * 关掉的影响只在以后的转写——已经存下的分段仍然显示，用户关的是「还要不要做」，
 * 不是「把已经做过的结果删掉」。
 */
export default function NoteSettingsSection({
  profile,
  onSaved,
}: {
  profile: UserProfile | null;
  onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [settings, setSettings] = useState<NoteSettings>(DEFAULT_NOTE_SETTINGS);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setSettings(profile?.note_settings ?? DEFAULT_NOTE_SETTINGS);
  }, [profile?.note_settings]);

  const toggle = async () => {
    const next = { ...settings, diarization_enabled: !settings.diarization_enabled };
    setSettings(next);
    setSaving(true);
    try {
      await setNoteDiarizationEnabled(next.diarization_enabled);
      onSaved();
    } catch {
      setSettings(profile?.note_settings ?? DEFAULT_NOTE_SETTINGS);
      onSaved();
      toast.error(t("settings.noteSettingsSaveFailed"));
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="settings-card" data-nav-id="note-settings">
      <div className="settings-section-header">
        <Users size={15} className="icon-accent" />
        <h2 className="settings-section-title">{t("settings.noteSettings")}</h2>
      </div>
      <div className="settings-column history-settings-column">
        <p className="settings-hint settings-hint-flush">{t("settings.noteSettingsHint")}</p>

        <div className="settings-row">
          <div className="settings-column history-setting-copy">
            <span className="permission-label">{t("settings.noteDiarization")}</span>
            <span className="settings-hint settings-hint-flush">
              {t("settings.noteDiarizationHint")}
            </span>
          </div>
          <button
            type="button"
            role="switch"
            aria-checked={settings.diarization_enabled}
            aria-label={t("settings.noteDiarization")}
            className="toggle-switch"
            data-active={settings.diarization_enabled}
            disabled={saving}
            onClick={() => void toggle()}
          >
            <div className="toggle-knob" />
          </button>
        </div>
      </div>
    </section>
  );
}
