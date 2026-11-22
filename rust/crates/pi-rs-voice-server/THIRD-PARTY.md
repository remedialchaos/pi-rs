The Supertonic text and inference routines were adapted from the [Supertone helper](https://github.com/supertone-inc/supertonic/blob/main/py/helper.py) vendored by p2. Copyright (c) 2025 Supertone Inc.

The fallback Kokoro vocabulary is from [kokoro-onnx](https://github.com/thewh1teagle/kokoro-onnx/blob/main/src/kokoro_onnx/config.json). Copyright (c) 2025 github.com/thewh1teagle.

MIT License (applies separately to both notices above):

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the “Software”), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

NeMo feature setup, TDT scheduling, and Silero endpoint behavior were checked against [sherpa-onnx v1.13.8](https://github.com/k2-fsa/sherpa-onnx/tree/v1.13.8/sherpa-onnx/csrc), Apache-2.0. At runtime Kokoro calls the platform's separately installed [eSpeak NG](https://github.com/espeak-ng/espeak-ng) shared library. Neither the shared libraries nor any model weights are shipped by this crate.
