// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

import { RuntimeRefused } from "./gate.ts";

/** Keep the durable language snapshot schema for recovery. A legacy reply
 * locale is metadata only; ordinary replies are determined by the model
 * from the user's request, not the application's language preference. */
export interface TaskLanguages {
  readonly interfaceLanguage: string;
  readonly replyLanguage: string;
  readonly contentLanguage: string;
}

const languages: Readonly<Record<string, string>> = Object.freeze({
  en: "English", ar: "Arabic", az: "Azerbaijani", bn: "Bengali", de: "German",
  es: "Spanish", fa: "Persian", fr: "French", he: "Hebrew", hi: "Hindi",
  hr: "Croatian", id: "Indonesian", it: "Italian", ja: "Japanese", ko: "Korean",
  nl: "Dutch", pl: "Polish", "pt-BR": "Brazilian Portuguese", ru: "Russian",
  th: "Thai", tr: "Turkish", uk: "Ukrainian", ur: "Urdu", vi: "Vietnamese",
  "zh-Hans": "Simplified Chinese", "zh-TW": "Traditional Chinese",
});

export function taskLanguages(value: unknown): TaskLanguages {
  // Legacy native submissions predate language metadata and used Chinese.
  if (value === undefined) return Object.freeze({
    interfaceLanguage: "zh-Hans", replyLanguage: "zh-Hans", contentLanguage: "zh-Hans",
  });
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new RuntimeRefused("invalid task language snapshot");
  }
  const input = value as Record<string, unknown>;
  const keys = ["interfaceLanguage", "replyLanguage", "contentLanguage"] as const;
  if (Object.keys(input).length !== keys.length
    || keys.some((key) => typeof input[key] !== "string"
      || (!(key === "replyLanguage" && input[key] === "auto")
        && !Object.hasOwn(languages, input[key] as string)))) {
    throw new RuntimeRefused("invalid task language snapshot");
  }
  return Object.freeze({
    interfaceLanguage: input.interfaceLanguage as string,
    replyLanguage: input.replyLanguage as string,
    contentLanguage: input.contentLanguage as string,
  });
}

export function languageInstructions(snapshot: TaskLanguages, writing: boolean): string {
  const content = languages[snapshot.contentLanguage];
  return [
    "Follow the user's language requirements for replies; otherwise respond naturally in the language best suited to the request.",
    writing ? ("Use " + content + " for writing candidates unless the user requests a different candidate language.") : "",
    "Do not translate or rewrite historical messages, saved drafts, filenames or identifiers unless the user explicitly requests it.",
    writing ? "For this writing task, the final answer must contain only the candidate text in its candidate language." : "",
  ].filter(Boolean).join("\n");
}
