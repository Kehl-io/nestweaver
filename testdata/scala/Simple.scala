import scala.collection.mutable.ListBuffer

trait Greeter {
  def greet(name: String): String
}

class SimpleGreeter extends Greeter {
  override def greet(name: String): String = {
    s"Hello, $name!"
  }

  private def formatName(name: String): String = {
    name.capitalize
  }
}

object AppConfig {
  val defaultGreeting: String = "Hello"

  def createGreeter(): Greeter = {
    new SimpleGreeter()
  }
}

def main(args: Array[String]): Unit = {
  val greeter = AppConfig.createGreeter()
  println(greeter.greet("World"))
}
