import { core } from "ext:core/mod.js";

const {
  ObjectDefineProperties,
  SymbolFor,
} = globalThis.__bootstrap.primordials;

// flow(2.9.0): `00_webidl.js` is a `lazy_loaded_js` script, so pull it via
// `core.loadExtScript` instead of a static ESM `import` (see ext/web/06_streams.js).
const webidl = core.loadExtScript("ext:deno_webidl/00_webidl.js");

class Navigator {
  constructor() {
    webidl.illegalConstructor();
  }

  [SymbolFor("Deno.privateCustomInspect")](inspect) {
    return `${this.constructor.name} ${inspect({})}`;
  }
}

const navigator = webidl.createBranded(Navigator);

let numCpus, userAgent, language;

function setNumCpus(val) {
  numCpus = val;
}

function setUserAgent(val) {
  userAgent = val;
}

function setLanguage(val) {
  language = val;
}

ObjectDefineProperties(Navigator.prototype, {
  hardwareConcurrency: {
    configurable: true,
    enumerable: true,
    get() {
      webidl.assertBranded(this, NavigatorPrototype);
      return numCpus;
    },
  },
  userAgent: {
    configurable: true,
    enumerable: true,
    get() {
      webidl.assertBranded(this, NavigatorPrototype);
      return userAgent;
    },
  },
  language: {
    configurable: true,
    enumerable: true,
    get() {
      webidl.assertBranded(this, NavigatorPrototype);
      return language;
    },
  },
  languages: {
    configurable: true,
    enumerable: true,
    get() {
      webidl.assertBranded(this, NavigatorPrototype);
      return [language];
    },
  },
});
const NavigatorPrototype = Navigator.prototype;

export { Navigator, navigator, setLanguage, setNumCpus, setUserAgent };
