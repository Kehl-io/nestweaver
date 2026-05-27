module math_utils
  implicit none

  type :: Vector3D
    real :: x, y, z
  end type Vector3D

contains

  function add_vectors(a, b) result(c)
    type(Vector3D), intent(in) :: a, b
    type(Vector3D) :: c
    c%x = a%x + b%x
    c%y = a%y + b%y
    c%z = a%z + b%z
  end function add_vectors

  subroutine normalize(v)
    type(Vector3D), intent(inout) :: v
    real :: mag
    mag = sqrt(v%x**2 + v%y**2 + v%z**2)
    if (mag > 0.0) then
      v%x = v%x / mag
      v%y = v%y / mag
      v%z = v%z / mag
    end if
  end subroutine normalize

end module math_utils

program main
  use math_utils
  implicit none

  type(Vector3D) :: a, b, c

  a = Vector3D(1.0, 2.0, 3.0)
  b = Vector3D(4.0, 5.0, 6.0)

  c = add_vectors(a, b)
  call normalize(c)

  print *, "Result:", c%x, c%y, c%z
end program main
