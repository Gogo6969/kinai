// SpeechRecognition isn't part of the standard DOM lib yet — TS doesn't ship
// the type. Browsers expose it on `window.SpeechRecognition` or the WebKit-
// prefixed `window.webkitSpeechRecognition`.
//
// macOS WKWebView supports the API on macOS 13+ (Sonoma) but routes
// recognition through Apple's on-device speech model — audio doesn't leave
// the machine.

interface SpeechRecognitionAlternative {
  transcript: string;
  confidence: number;
}
interface SpeechRecognitionResult {
  readonly length: number;
  readonly isFinal: boolean;
  [index: number]: SpeechRecognitionAlternative;
}
interface SpeechRecognitionResultList {
  readonly length: number;
  [index: number]: SpeechRecognitionResult;
}
interface SpeechRecognitionEvent extends Event {
  readonly results: SpeechRecognitionResultList;
  readonly resultIndex: number;
}
interface SpeechRecognitionErrorEvent extends Event {
  readonly error: string;
  readonly message: string;
}
export interface SpeechRecognitionLike extends EventTarget {
  continuous: boolean;
  interimResults: boolean;
  lang: string;
  maxAlternatives: number;
  onresult: ((this: SpeechRecognitionLike, ev: SpeechRecognitionEvent) => void) | null;
  onerror: ((this: SpeechRecognitionLike, ev: SpeechRecognitionErrorEvent) => void) | null;
  onend: ((this: SpeechRecognitionLike) => void) | null;
  onstart: ((this: SpeechRecognitionLike) => void) | null;
  start(): void;
  stop(): void;
  abort(): void;
}

type SRConstructor = new () => SpeechRecognitionLike;

declare global {
  interface Window {
    SpeechRecognition?: SRConstructor;
    webkitSpeechRecognition?: SRConstructor;
  }
}

export function speechRecognitionAvailable(): boolean {
  return typeof window !== 'undefined' &&
    !!(window.SpeechRecognition || window.webkitSpeechRecognition);
}

export function createRecognition(): SpeechRecognitionLike | null {
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SR) return null;
  const rec = new SR();
  rec.continuous = true;
  rec.interimResults = true;
  rec.maxAlternatives = 1;
  rec.lang = navigator.language || 'en-US';
  return rec;
}
