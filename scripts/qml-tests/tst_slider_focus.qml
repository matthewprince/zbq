import QtQuick
import QtTest
import com.blitzfc.qbz
import "../../crates/qbz-qt/qml/controls" as Controls

Item {
    width: 400; height: 120
    Controls.QbzSlider { id: slider; x: 40; y: 40; minimum: 0; maximum: 100; value: 50 }
    Item { id: elsewhere; activeFocusOnTab: true; width: 10; height: 10 }
    TestCase {
        name: "SliderFocusRing"
        when: windowShown
        function ring() { return findChild(slider, "qbzSliderFocusRing") }
        function init() { elsewhere.forceActiveFocus() }
        function test_click_takes_focus_without_leaving_a_ring() {
            mouseClick(slider, 100, 11)
            verify(slider.activeFocus, "a click still focuses the slider for the arrow keys")
            compare(ring().border.width, 0)
        }
        function test_keys_after_a_click_bring_the_ring_back() {
            mouseClick(slider, 100, 11)
            keyClick(Qt.Key_Right)
            compare(ring().border.width, 2)
        }
        function test_keyboard_focus_shows_the_ring() {
            mouseClick(slider, 100, 11)
            elsewhere.forceActiveFocus()
            slider.forceActiveFocus(Qt.TabFocusReason)
            compare(ring().border.width, 2)
        }
    }
}
