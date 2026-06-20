import app from "ags/gtk4/app"
import style from "./style.css"
import Dashboard from "./widget/Dashboard"

app.start({
  css: style,
  main() {
    Dashboard()
  },
})
