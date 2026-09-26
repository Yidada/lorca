import { Alert } from "react-native";
import { t } from "../i18n";
import { Row } from "./forms";

export interface ClaudeSelection { model?: string; thinking?: string }

export function ClaudeSettings({ selection, onChange }: {
  selection: ClaudeSelection;
  onChange: (value: ClaudeSelection) => void | Promise<void>;
}) {
  const models: (string | undefined)[] = [undefined, "opus", "sonnet", "haiku"];
  if (selection.model && !models.includes(selection.model)) models.push(selection.model);
  const levels: (string | undefined)[] = [undefined, "low", "medium", "high", "xhigh", "max"];
  const change = async (value: ClaudeSelection) => {
    try { await onChange(value); }
    catch (error) { Alert.alert(t("Could not update the bot"), String(error)); }
  };
  return <>
    <Row title={t("Model")} menu={{ title: t("Model"), value: selection.model ?? t("Runner default"),
      choices: models.map((model) => ({ title: model ?? t("Runner default"), selected: selection.model === model,
        onPress: () => change({ model, thinking: undefined }) })) }} />
    <Row title={t("Thinking")} menu={{ title: t("Thinking"), value: selection.thinking ?? t("Runner default"),
      choices: levels.map((thinking) => ({ title: thinking ?? t("Runner default"), selected: selection.thinking === thinking,
        onPress: () => change({ ...selection, thinking }) })) }} />
    <Row title="Claude Code" subtitleLines={0} subtitle={t("Uses Claude Code on this Runner. Sign in there first. Available models and thinking levels depend on its version and account.")} />
  </>;
}

